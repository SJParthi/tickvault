//! User + Exceptions (readiness) — §39.3 row 5 of the Groww order-side area
//! collision contract. Readiness REUSES `SPOT1M-01`
//! ([`ErrorCode::Spot1m01FetchDegraded`]) with `feed="groww"`,
//! `leg="readiness"`, `stage` fields; the reserved `GROWW-READY-*` prefix is
//! deliberately UNUSED (zero new ErrorCode variants).
//!
//! # Lattice note (§39.2)
//! This module compiles only under the non-default `groww_orders` cargo
//! feature (Gate 2, inherited from `oms/mod.rs` — no per-file cfg). It is a
//! READ-ONLY GET surface: it fires only when the injected enable is true AND
//! inside market hours (§10.1); no mutation, no order, `dry_run` untouched.
//! `GROWW_ORDER_LIVE_FIRE` is irrelevant by construction — nothing mutating
//! exists here to gate; stated, not consulted.
//!
//! # The user-detail ban
//! The other documented user-surface GET (doc-16 §1 row 19) has NO documented
//! response schema — consuming it is BANNED (no-invention rule); this module
//! deliberately does not carry its path string at all.
//!
//! # Probe-endpoint ownership
//! `GET /v1/margins/detail/user` is the §10.2 margin-area row consumed by the
//! user area as the readiness probe (the only fully-documented authenticated
//! GET — 25-field verbatim schema, `docs/groww-ref/16-orders-margins-portfolio.md`
//! §3.1). The gating flag (`user_read` vs `margin_read`) is injected by the
//! caller — the build-lead wiring PR records the ruling. `margin.rs` will
//! separately own the full typed margins client; NO inter-area code dependency.
//!
//! # Cadence lineage (review-H1 honesty carry)
//! The prior design fired 08:50 IST (the Dhan 08:45 precedent); the operator's
//! §10 grant conditions read-only GETs on market-hours-only, so the default is
//! 09:15:30 IST. Flip to 08:50 = ONE const change
//! ([`READINESS_FIRE_SECS_OF_DAY_IST`]) + a fresh dated §39 amendment FIRST.
//! Honest cost: the in-session default is a market-open readiness
//! confirmation, not a pre-market warning.
//!
//! # The alive-until-06:00 inference (review-M1 residuals, all four)
//! A 2xx SUCCESS proves the token alive RIGHT NOW; "alive now ⇒ alive until
//! tomorrow 06:00" is Assumed on (a) the Verified daily-expiry model, (b) U-17
//! one-active-token, (c) U-18 reset shape, (d) UI revocation at any time
//! (17-token-lifecycle §1/§5) + server-side invalidation with zero mints (the
//! 2026-07-06 Dhan incident class behind AUTH-GAP-05). A heuristic, never a
//! guarantee. The report carries its own `fired_secs_of_day_ist` freshness
//! stamp (§38.8 decision-freshness analog).
//!
//! # The zero-REST operator option (review-M2 carry)
//! The bruteX minter Lambda could publish the mint response's
//! `expiry`/`isActive` alongside the token in SSM (bruteX writes, TickVault
//! reads); cross-repo follow-up, needs a dated note in
//! `groww-shared-token-minter-2026-07-02.md`.
//!
//! # Forensics wiring contract (review-M3 carry)
//! Future app-layer `rest_fetch_audit` rows use sentinels `security_id = 0`,
//! `exchange_segment = 'NA'`, `symbol = 'READINESS'`
//! (= [`READINESS_AUDIT_SYMBOL`]), `leg = 'readiness'`; collision-free (leg
//! unique; one fire/day/outcome; `outcome` is in the table's DEDUP key).
//!
//! # token OK ≠ feed entitled
//! No entitlement probe exists (U-13); the feed's FEED-STALL-01 never-streamed
//! arm is the de-facto detector.
//!
//! # Exceptions home (§39.3 — the canonical order-side failure classifier)
//! [`GaFailure`] + [`sniff_ga_failure`] + [`classify_response`] are the
//! CANONICAL Groww order-side failure classifier. Decision rules, condensed:
//!
//! | # | Rule |
//! |---|------|
//! | 1 | HTTP 401/403 → auth-class; STATUS wins even over a SUCCESS body |
//! | 2 | RED (auth) is UNREACHABLE from body content alone — HTTP-status-proven only |
//! | 3 | The FAILURE envelope wins over any OTHER HTTP status, 2xx included |
//! | 4 | `ga_code` is FORENSICS ONLY — no policy branches on it (the #1539/G1 rule) |
//! | 5 | Vendor rejects (possible VAL-class) are NEVER blind-retried (DH-905/906 discipline) |
//! | 6 | A bare non-2xx without an envelope is `http_error`, retryable iff ≥500 |
//! | 7 | Unknown/undecodable shapes are LOUD transients, never silent, never auth-on-ambiguity (F12/AUTH-GAP-05 lesson) |
//! | 8 | Every body sample is redacted + bounded BEFORE storage (`capture_rest_error_body`) |
//!
//! Sibling area modules (`api_client.rs`, `portfolio.rs`, `margin.rs`,
//! `smart_orders.rs`) MUST import these instead of re-deriving. Explicitly
//! OUT of scope: order-lifecycle statuses (the #1549 `BrokerOrderStatus`
//! seam), SDK exception classes (sidecar surface), NATS/feed surfaces, GA007
//! semantics (order-path only, future).
//!
//! # Rate-limit honesty (review-L2 carry)
//! The `/margins/*` family is NOT ENUMERATED in 15-rate-limits §3
//! (conservative Unknown; 16-§1 says "Non Trading" flatly — the two docs
//! conflict, noted); ≤2 GETs/day is ~0% of any bucket; no per-day cap exists
//! (Verified-absence).
//!
//! # Why `Spot1m02PersistFailed` is NOT used
//! This module persists nothing — there is no persist leg to fail; reserved
//! for the future app-layer forensics wiring.
//!
//! # Delivery boundary (honest)
//! Counters are metrics-local + emits are log-sink-only today; the operator
//! signal is the future typed Telegram event (build-lead events.rs request) —
//! the FEED-REJECT-01/SCOREBOARD-01 precedent. [`ReadinessOutcome::operator_summary`]
//! is the ready-made body.
//!
//! # Honest envelope
//! 100% inside the tested envelope, with ratcheted regression coverage: a
//! total, no-panic, proptest-pinned classifier over every documented +
//! adversarial (status, body) permutation; a bounded ≤2-attempt,
//! 5s-per-request, once-per-trading-day probe with a backwards-clock-proof
//! day latch, that fail-soft degrades LOUD (one coalesced coded emit) and
//! gates NOTHING. NOT claimed: live Groww behaviour (the margins GET is
//! UNVERIFIED-LIVE from this module — paper-tested against docs/groww-ref/16
//! only); feed/data entitlement; F&O-enabled semantics (`fno_margin_details`
//! absence NOT DOCUMENTED — observation only); the alive-until-06:00
//! inference (Assumed, four named residuals); pre-open warning. O(1): the
//! latch claim, verdict fold, and slug map are O(1); the classifier is
//! O(body ≤ 64 KiB) — flagged, not claimed O(1).

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use tickvault_common::constants::{
    GROWW_API_VERSION_HEADER, GROWW_API_VERSION_VALUE, IST_UTC_OFFSET_SECONDS_I64,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::market_hours::is_within_market_hours_ist;
use tickvault_common::sanitize::capture_rest_error_body;
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// §A — consts (all module-local; pub only where the wiring needs them)
// ---------------------------------------------------------------------------

/// Daily fire time: 09:15:30 IST (market-hours-only per the §10 grant).
///
/// Flipping to the 08:50 IST pre-open slot is ONE change to this const —
/// but requires a fresh dated §39 amendment FIRST (the market-hours-only
/// condition is the operator's; `test_fire_const_is_market_hours_compliant`
/// fails a silent flip).
pub const READINESS_FIRE_SECS_OF_DAY_IST: u32 = 9 * 3600 + 15 * 60 + 30;

/// Late-boot catch-up window end: 15:30:00 IST — the probe fires once per
/// trading day inside `[09:15:30, 15:30:00)`, never after close.
pub const READINESS_FIRE_WINDOW_END_SECS_IST: u32 = 15 * 3600 + 30 * 60;

/// The ONE bounded in-run retry gap (09:15:30 → 09:20:30). Trivially ≥ the
/// caller's 60s token-re-read floor (`groww-shared-token-minter-2026-07-02.md`).
pub const READINESS_RETRY_DELAY_SECS: u64 = 300;

/// Per-request bound (the house SPOT1M discipline; the SDK `timeout=None`
/// class is banned).
const READINESS_REQUEST_TIMEOUT_SECS: u64 = 5;

/// Attempt 1 + one bounded retry — never a loop.
const READINESS_MAX_ATTEMPTS: u8 = 2;

/// Body cap: the documented payload is ~1–2 KB; a hostile/looping gateway is
/// refused at 64 KiB (declared-length pre-check + accumulation cap — the
/// tf-verify precedent).
const READINESS_MAX_BODY_BYTES: usize = 64 * 1024;

/// The probe endpoint — the ONLY `/v1/` literal in this module (Gate-5
/// sanctioned file; ratcheted by `test_ratchet_single_v1_literal_and_no_user_detail`).
const READINESS_MARGIN_ENDPOINT: &str = "/v1/margins/detail/user";

/// Production base URL — exported so the wiring PR passes THIS const; tests
/// pass localhost fixtures.
pub const READINESS_DEFAULT_BASE_URL: &str = "https://api.groww.in"; // APPROVED: injectable base-url const (module reads no config per §39.3; tests inject localhost)

/// Emit field value (storage's `SPOT_1M_REST_FEED_GROWW` is unreachable from
/// trading; value equality pinned by `test_readiness_feed_leg_literals_pinned`).
const READINESS_FEED: &str = "groww";

/// The grep-split emit field distinguishing readiness lines from the spot /
/// chain / contract legs sharing the SPOT1M codes.
const READINESS_LEG: &str = "readiness";

/// The review-M3 forensics sentinel `symbol` for future `rest_fetch_audit`
/// rows — exported for the wiring PR.
pub const READINESS_AUDIT_SYMBOL: &str = "READINESS";

/// GA wire codes are 5 chars (`GA005`); 16 bounds a hostile field (the #1539
/// sanitizer mirror).
const GA_CODE_MAX_CHARS: usize = 16;

/// Headline top-level doc fields tracked for schema-drift counting.
const HEADLINE_FIELD_COUNT: u8 = 6;

/// Rupee plausibility bound: non-finite or |x| > 1e12 values are dropped +
/// counted absent (display/forensics-only values, never a decision input).
const MAX_PLAUSIBLE_RUPEES: f64 = 1e12;

// ---------------------------------------------------------------------------
// §B — pure core (zero I/O, total, no-panic)
// ---------------------------------------------------------------------------

/// Groww failure-envelope forensics (doc-16 §5). The envelope WINS over the
/// HTTP status EXCEPT 401/403 (auth short-circuit is HTTP-status-only — the
/// #1539/G1 rule). `ga_code` is sanitized to the #1539 house shape and is
/// FORENSICS ONLY — no policy anywhere branches on it. GA002 does not exist;
/// unknown codes carry verbatim-after-sanitize (annexure rule-15 discipline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GaFailure {
    /// `[A-Za-z0-9_-]` filtered, ≤ `GA_CODE_MAX_CHARS`, `"none"` when
    /// absent/empty.
    pub ga_code: String,
    /// Pre-redacted ≤300 chars via `capture_rest_error_body`.
    pub message_sample: String,
}

/// Total, no-panic sniff: `Some(GaFailure)` iff the body is JSON whose
/// top-level `status` == `"FAILURE"`. SUCCESS / absent status / non-JSON →
/// `None`. Mirror of #1539's app-side sniffer (crates/app is unimportable
/// from trading); copy-drift pinned by identical test vectors
/// (`test_ga_sanitizer_vectors_match_1539`).
pub fn sniff_ga_failure(body: &str) -> Option<GaFailure> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v.get("status").and_then(|s| s.as_str()) != Some("FAILURE") {
        return None;
    }
    let err = v.get("error");
    // Sanitize BEFORE bounding (the #1539 security-review rule): the code
    // flows into tracing fields, so only `[A-Za-z0-9_-]` survives — no
    // newlines/ANSI/BiDi/quotes can reach a log line. Empty (absent or
    // fully filtered) → "none".
    let ga_code = err
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .map(|c| {
            c.chars()
                .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
                .take(GA_CODE_MAX_CHARS)
                .collect::<String>()
        })
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "none".to_string());
    let message_sample = capture_rest_error_body(
        err.and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or_default(),
    );
    Some(GaFailure {
        ga_code,
        message_sample,
    })
}

/// Nested F&O margin object (6 fields, doc-16 §3.1). EVERY field is
/// `Option` + `serde(default)` — absence degrades, never a parse failure
/// (capture-staleness caveat).
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct FnoMarginDetails {
    #[serde(default)]
    pub net_fno_margin_used: Option<f64>,
    #[serde(default)]
    pub span_margin_used: Option<f64>,
    #[serde(default)]
    pub exposure_margin_used: Option<f64>,
    #[serde(default)]
    pub future_balance_available: Option<f64>,
    #[serde(default)]
    pub option_buy_balance_available: Option<f64>,
    #[serde(default)]
    pub option_sell_balance_available: Option<f64>,
}

/// Nested equity margin object (5 fields, doc-16 §3.1) — same defensive
/// pattern as [`FnoMarginDetails`].
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct EquityMarginDetails {
    #[serde(default)]
    pub net_equity_margin_used: Option<f64>,
    #[serde(default)]
    pub cnc_margin_used: Option<f64>,
    #[serde(default)]
    pub mis_margin_used: Option<f64>,
    #[serde(default)]
    pub cnc_balance_available: Option<f64>,
    #[serde(default)]
    pub mis_balance_available: Option<f64>,
}

/// Nested commodity margin object (7 fields, doc-16 §3.1) — same defensive
/// pattern as [`FnoMarginDetails`].
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct CommodityMarginDetails {
    #[serde(default)]
    pub commodity_span_margin: Option<f64>,
    #[serde(default)]
    pub commodity_exposure_margin: Option<f64>,
    #[serde(default)]
    pub commodity_tender_margin: Option<f64>,
    #[serde(default)]
    pub commodity_special_margin: Option<f64>,
    #[serde(default)]
    pub commodity_additional_margin: Option<f64>,
    #[serde(default)]
    pub commodity_unrealised_m2m: Option<f64>,
    #[serde(default)]
    pub commodity_realised_m2m: Option<f64>,
}

/// Top-level margin payload. Doc-16 §3.1's schema TABLE flattens the three
/// `*_margin_details` objects while its prose states nesting — wire truth
/// Unknown, so the reader is DUAL-SHAPE: nested object first, top-level
/// flattened keys as fallback for the headline fields. Rupee values are
/// display/forensics-only `f64` (deliberate deviation from the
/// `margin_gate.rs` paise discipline: NO decision path consumes these —
/// §28 frozen, §38.8 analog).
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct MarginPayload {
    #[serde(default)]
    pub clear_cash: Option<f64>,
    #[serde(default)]
    pub net_margin_used: Option<f64>,
    #[serde(default)]
    pub brokerage_and_charges: Option<f64>,
    #[serde(default)]
    pub collateral_used: Option<f64>,
    #[serde(default)]
    pub collateral_available: Option<f64>,
    #[serde(default)]
    pub adhoc_margin: Option<f64>,
    #[serde(default)]
    pub fno_margin_details: Option<FnoMarginDetails>,
    #[serde(default)]
    pub equity_margin_details: Option<EquityMarginDetails>,
    #[serde(default)]
    pub commodity_margin_details: Option<CommodityMarginDetails>,
    /// Flattened-shape fallback (dual-shape read).
    #[serde(default)]
    pub option_buy_balance_available: Option<f64>,
}

/// Every probe failure class — named, never silent (Rule 11).
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeFailure {
    /// HTTP 401/403 — auth-class, HTTP-STATUS-PROVEN ONLY. Status wins even
    /// over a SUCCESS body (broken-proxy contradiction pin). The ONLY class
    /// that signals token re-read. GA captured as forensics.
    Auth {
        /// The proving HTTP status (401 or 403).
        http_status: u16,
        /// Sanitized GA wire code, `"none"` when absent.
        ga_code: String,
        /// Redacted ≤300-char body sample.
        body_sample: String,
    },
    /// HTTP 429. Retry-After parsed defensively-if-present (Verified-ABSENT
    /// in docs; sidecar precedent), capped at 300s; one bounded retry, never
    /// out-polled.
    RateLimited {
        /// Always 429.
        http_status: u16,
        /// The capped Retry-After header value when the server supplied one.
        retry_after_secs: Option<u64>,
    },
    /// FAILURE envelope on any NON-auth, non-429 status — INCLUDING 2xx
    /// (envelope wins; the G1 lesson). NEVER retried in-run (possible
    /// VAL-class — DH-905/906 discipline).
    VendorReject {
        /// The HTTP status the envelope arrived on.
        http_status: u16,
        /// Sanitized GA wire code, `"none"` when absent.
        ga_code: String,
        /// Redacted ≤300-char envelope message sample.
        body_sample: String,
    },
    /// Other non-2xx WITHOUT a decodable FAILURE envelope (400/404 static-req
    /// drift, 5xx, 418, …). Retryable iff status ≥ 500.
    HttpError {
        /// The bare HTTP status.
        http_status: u16,
        /// Redacted ≤300-char body sample.
        body_sample: String,
    },
    /// 2xx that is not JSON / carries no recognizable envelope / was
    /// truncated at the body cap. Loud, never a false GREEN.
    Parse {
        /// The 2xx status the unusable body arrived on.
        http_status: u16,
        /// Redacted ≤300-char detail.
        detail: String,
    },
    /// Declared Content-Length or accumulated body exceeded
    /// `READINESS_MAX_BODY_BYTES` (hostile/looping gateway; the tf-verify
    /// precedent). Body never fully read.
    Oversize {
        /// The declared Content-Length when the refusal was pre-read;
        /// `None` when the accumulation cap tripped mid-stream.
        declared_len: Option<u64>,
    },
    /// Send-leg / timeout / DNS / TLS / request-build. `detail` = OUR error
    /// string, redacted + bounded via `capture_rest_error_body`.
    Transport {
        /// Redacted ≤300-char transport error detail.
        detail: String,
    },
    /// The injected `fetch_token` closure returned `None` —
    /// credentials-class (the `GrowwAuthSmokeError::Credentials` split:
    /// NEVER auth-rejected).
    TokenUnavailable,
}

impl ProbeFailure {
    /// The stage slug for emits + future audit rows (static, closed set).
    pub fn stage(&self) -> &'static str {
        match self {
            Self::Auth { .. } => "auth",
            Self::RateLimited { .. } => "rate_limited",
            Self::VendorReject { .. } => "vendor_reject",
            Self::HttpError { .. } => "http_error",
            Self::Parse { .. } => "parse",
            Self::Oversize { .. } => "oversize",
            Self::Transport { .. } => "transport",
            Self::TokenUnavailable => "token_read",
        }
    }

    /// True ONLY for [`ProbeFailure::Auth`] — the caller must drop + re-read
    /// the SSM token (≥60s floor caller-owned; NEVER mint — the token-minter
    /// lock).
    pub fn requires_token_reread(&self) -> bool {
        matches!(self, Self::Auth { .. })
    }

    /// In-run retry policy (HTTP-status/shape-driven, GA-BLIND):
    /// Auth → true (the re-invoked `fetch_token` closure may deliver a fresh
    /// token — SSM stays caller-owned); RateLimited | Transport | Parse |
    /// TokenUnavailable → true; HttpError → status ≥ 500; VendorReject |
    /// Oversize → false.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Auth { .. }
            | Self::RateLimited { .. }
            | Self::Parse { .. }
            | Self::Transport { .. }
            | Self::TokenUnavailable => true,
            Self::HttpError { http_status, .. } => *http_status >= 500,
            Self::VendorReject { .. } | Self::Oversize { .. } => false,
        }
    }

    /// Emit helper: the failure's HTTP status, 0 when no wire status exists.
    fn http_status_or_zero(&self) -> u16 {
        match self {
            Self::Auth { http_status, .. }
            | Self::RateLimited { http_status, .. }
            | Self::VendorReject { http_status, .. }
            | Self::HttpError { http_status, .. }
            | Self::Parse { http_status, .. } => *http_status,
            Self::Oversize { .. } | Self::Transport { .. } | Self::TokenUnavailable => 0,
        }
    }

    /// Emit helper: the sanitized GA code, `"none"` when absent.
    fn ga_code_str(&self) -> &str {
        match self {
            Self::Auth { ga_code, .. } | Self::VendorReject { ga_code, .. } => ga_code,
            _ => "none",
        }
    }

    /// Emit helper: the redacted detail/body sample, `""` when none exists.
    fn detail_str(&self) -> &str {
        match self {
            Self::Auth { body_sample, .. }
            | Self::VendorReject { body_sample, .. }
            | Self::HttpError { body_sample, .. } => body_sample,
            Self::Parse { detail, .. } | Self::Transport { detail } => detail,
            Self::RateLimited { .. } | Self::Oversize { .. } | Self::TokenUnavailable => "",
        }
    }
}

/// Fail-closed pre-flight refusals — zero HTTP is issued on either arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessRefusal {
    /// The injected enable is false (the caller reads `[groww_orders]`; this
    /// module never reads config).
    Disabled,
    /// Outside market hours (§10.1 — enforced HERE, defense-in-depth, via
    /// `tickvault_common::market_hours::is_within_market_hours_ist`).
    OffHours,
}

/// The typed verdict the (future) wiring consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessVerdict {
    /// The token opened the margins page — login proven RIGHT NOW.
    Green,
    /// The broker HTTP-status-refused the token (401/403) — auth is dead.
    Red,
    /// The probe could not complete — one morning's signal lost, nothing gated.
    Degraded,
}

impl ReadinessVerdict {
    /// Static label for counters + emits.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red => "red",
            Self::Degraded => "degraded",
        }
    }
}

/// OBSERVATION only — never an entitlement claim (absence semantics for a
/// non-F&O account are NOT DOCUMENTED; a day-0 baseline on the known-F&O
/// account is required first).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FnoMarginObservation {
    /// The nested `fno_margin_details` object was present.
    Present,
    /// The nested object was absent from an otherwise-parsed payload.
    #[default]
    Absent,
    /// The payload itself was unusable — presence unknowable.
    PayloadUnparsed,
}

impl FnoMarginObservation {
    /// Static label for emits + future audit rows.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Absent => "absent",
            Self::PayloadUnparsed => "payload_unparsed",
        }
    }
}

/// What a successful probe observed (forensics/display only — no decision
/// path consumes these; §28 frozen).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MarginObservations {
    /// Verbatim rupees; 0 is reported, never classified a failure.
    pub clear_cash_rupees: Option<f64>,
    /// Dual-shape read: nested `fno_margin_details` first, flattened fallback.
    pub option_buy_balance_rupees: Option<f64>,
    /// The F&O margin-object observation (never an entitlement claim).
    pub fno_margin: FnoMarginObservation,
    /// Headline doc fields absent from the payload (schema-drift signal).
    pub absent_headline_fields: u8,
    /// SUCCESS envelope but the payload was unusable — the verdict stays
    /// GREEN (the envelope proves the token); ONE warn-level line (stage
    /// `payload_parse`) names the degraded observation (Rule 11).
    pub payload_parse_failed: bool,
    /// Full defensively-parsed payload for the future forensics wiring.
    pub full_payload: MarginPayload,
}

/// One probe attempt's result.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeAttempt {
    /// 2xx SUCCESS — the token is proven alive right now.
    Success {
        /// What the payload carried (possibly degraded — see
        /// [`MarginObservations::payload_parse_failed`]). Boxed to keep the
        /// enum's variants size-balanced (cold path — one alloc/day).
        observations: Box<MarginObservations>,
        /// Wall-clock latency of this attempt.
        latency_ms: u64,
    },
    /// A named failure class.
    Failure {
        /// The classified failure.
        failure: ProbeFailure,
        /// Wall-clock latency of this attempt.
        latency_ms: u64,
    },
}

/// The structured outcome the (future) wiring consumes — Telegram body,
/// `rest_fetch_audit` row, and counters all derive from this. Returned, not
/// persisted. NO token-typed field exists on this struct (type-level leak
/// impossibility).
#[derive(Debug, Clone, PartialEq)]
pub struct ReadinessOutcome {
    /// The folded verdict (see [`verdict_for`]).
    pub verdict: ReadinessVerdict,
    /// `Some` iff the final attempt succeeded.
    pub observations: Option<MarginObservations>,
    /// The FINAL attempt's failure, when it failed.
    pub failure: Option<ProbeFailure>,
    /// Attempts actually made (1 or 2).
    pub attempts: u8,
    /// The final attempt's wall-clock latency.
    pub last_latency_ms: u64,
    /// Freshness stamp (§38.8 analog): when the probe actually ran.
    pub fired_secs_of_day_ist: u32,
}

impl ReadinessOutcome {
    /// 10-commandments-compliant plain-English body (the future Telegram
    /// text). GREEN discloses "confirms the LOGIN only — token OK ≠ feed
    /// entitled"; RED names the ~6:05 AM key machine + the Trading API
    /// subscription; a GA005 decorates with the subscription HINT (Assumed
    /// signature — U-13), never a definitive cause.
    pub fn operator_summary(&self) -> String {
        let when = format_ist_12h(self.fired_secs_of_day_ist);
        match self.verdict {
            ReadinessVerdict::Green => {
                let cash = self
                    .observations
                    .as_ref()
                    .and_then(|o| o.clear_cash_rupees)
                    .map(|c| format!(" Clear cash seen: Rs {c:.2}."))
                    .unwrap_or_default();
                let degraded_note = if self
                    .observations
                    .as_ref()
                    .is_some_and(|o| o.payload_parse_failed)
                {
                    " (The funds numbers could not be read this time — the login itself is fine.)"
                } else {
                    ""
                };
                format!(
                    "🟢 GROWW — order login check PASSED at {when}. The funds page \
                     answered, so today's key works. This confirms the LOGIN only — a \
                     working key does not prove the live price feed or any order \
                     permission.{cash}{degraded_note}"
                )
            }
            ReadinessVerdict::Red => {
                let hint = if self
                    .failure
                    .as_ref()
                    .is_some_and(|f| f.ga_code_str() == "GA005")
                {
                    " The broker's message hints the Trading API subscription may be \
                     missing or expired (our best guess, not a confirmed cause)."
                } else {
                    ""
                };
                format!(
                    "🆘 GROWW — order login check FAILED at {when}: the broker REFUSED \
                     today's key. Check two things: 1. the morning key machine (runs \
                     about 6:05 AM) produced today's key, 2. the Groww Trading API \
                     subscription is still active.{hint} The live price feed is \
                     unaffected."
                )
            }
            ReadinessVerdict::Degraded => {
                let reason = self
                    .failure
                    .as_ref()
                    .map_or("the check did not run", plain_english_reason);
                format!(
                    "⚠️ GROWW — order login check could NOT complete at {when} \
                     ({reason}). One morning's readiness signal is lost — nothing is \
                     gated, and the live price feed is unaffected. It will try again \
                     tomorrow."
                )
            }
        }
    }

    /// `"ok"` | the failure stage slug — the future audit-row `outcome` value.
    pub fn audit_outcome_slug(&self) -> &'static str {
        match &self.failure {
            None => "ok",
            Some(failure) => failure.stage(),
        }
    }

    /// Delegates to the final failure's
    /// [`ProbeFailure::requires_token_reread`].
    pub fn requires_token_reread(&self) -> bool {
        self.failure
            .as_ref()
            .is_some_and(ProbeFailure::requires_token_reread)
    }
}

/// Plain-English degraded reason for the operator summary (no library names,
/// no jargon — the 10 Telegram commandments).
fn plain_english_reason(failure: &ProbeFailure) -> &'static str {
    match failure {
        ProbeFailure::Auth { .. } => "the broker refused the key",
        ProbeFailure::RateLimited { .. } => "the broker asked us to slow down",
        ProbeFailure::VendorReject { .. } => "the broker rejected the request",
        ProbeFailure::HttpError { .. } => "the broker's server had a problem",
        ProbeFailure::Parse { .. } => "the broker's answer could not be read",
        ProbeFailure::Oversize { .. } => "the broker's answer was suspiciously large",
        ProbeFailure::Transport { .. } => "the connection could not be made",
        ProbeFailure::TokenUnavailable => "today's key was not available to us",
    }
}

/// 12-hour IST clock label for operator text ("9:15 AM" style — commandment 9).
fn format_ist_12h(secs_of_day: u32) -> String {
    let clamped = secs_of_day % 86_400;
    let hour24 = clamped / 3600;
    let minute = (clamped % 3600) / 60;
    let (hour12, meridiem) = match hour24 {
        0 => (12, "AM"),
        1..=11 => (hour24, "AM"),
        12 => (12, "PM"),
        _ => (hour24 - 12, "PM"),
    };
    format!("{hour12}:{minute:02} {meridiem} IST")
}

/// Total classifier: `(http_status, retry_after_secs, body)` → observations
/// or a named failure. Precedence ladder (pinned by the permutation tests):
/// 1. 401|403 → Auth (STATUS wins, even over a SUCCESS body — contradiction
///    pin; GA sniffed as forensics);
/// 2. 429 → RateLimited (Retry-After capped at 300s);
/// 3. body FAILURE envelope (any remaining status incl. 2xx) → VendorReject;
/// 4. 2xx + SUCCESS envelope (or a bare payload object — envelope-drift
///    defense) → defensive dual-shape parse → `Ok(MarginObservations)`
///    (absent fields counted; a wholly-unusable payload sets
///    `payload_parse_failed` — STILL `Ok`: the envelope proves the token);
/// 5. 2xx otherwise (non-JSON / truncated) → Parse;
/// 6. any other non-2xx → HttpError.
///
/// Oversize + Transport + TokenUnavailable are produced by the shell, not
/// here. Every body sample passes `capture_rest_error_body` BEFORE storage —
/// raw bodies never leave this fn.
pub fn classify_response(
    http_status: u16,
    retry_after_secs: Option<u64>,
    body: &str,
) -> Result<MarginObservations, ProbeFailure> {
    if http_status == 401 || http_status == 403 {
        let (ga_code, body_sample) = match sniff_ga_failure(body) {
            Some(ga) => (ga.ga_code, ga.message_sample),
            None => ("none".to_string(), capture_rest_error_body(body)),
        };
        return Err(ProbeFailure::Auth {
            http_status,
            ga_code,
            body_sample,
        });
    }
    if http_status == 429 {
        return Err(ProbeFailure::RateLimited {
            http_status,
            retry_after_secs: retry_after_secs.map(|s| s.min(READINESS_RETRY_DELAY_SECS)),
        });
    }
    if let Some(ga) = sniff_ga_failure(body) {
        return Err(ProbeFailure::VendorReject {
            http_status,
            ga_code: ga.ga_code,
            body_sample: ga.message_sample,
        });
    }
    if (200..300).contains(&http_status) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
            return Err(ProbeFailure::Parse {
                http_status,
                detail: capture_rest_error_body(body),
            });
        };
        let serde_json::Value::Object(ref obj) = value else {
            return Err(ProbeFailure::Parse {
                http_status,
                detail: "2xx JSON body is not an object".to_string(),
            });
        };
        let payload_value = match obj.get("status").and_then(|s| s.as_str()) {
            // Envelope shape: payload nested; the doc table's flattened shape
            // (fields beside `status`) falls back to the whole object.
            Some("SUCCESS") => obj.get("payload").cloned().unwrap_or_else(|| value.clone()),
            // A 2xx with an unrecognized envelope status is loud, never GREEN.
            Some(other) => {
                return Err(ProbeFailure::Parse {
                    http_status,
                    detail: capture_rest_error_body(&format!(
                        "unrecognized envelope status `{other}`"
                    )),
                });
            }
            // Bare payload object — envelope-drift defense.
            None => value.clone(),
        };
        return match serde_json::from_value::<MarginPayload>(payload_value) {
            Ok(payload) => Ok(observations_from_payload(payload)),
            // SUCCESS envelope, unusable payload: the envelope proves the
            // token — GREEN-degraded, flagged, never silent (Rule 11).
            Err(_) => Ok(MarginObservations {
                fno_margin: FnoMarginObservation::PayloadUnparsed,
                absent_headline_fields: HEADLINE_FIELD_COUNT,
                payload_parse_failed: true,
                ..MarginObservations::default()
            }),
        };
    }
    Err(ProbeFailure::HttpError {
        http_status,
        body_sample: capture_rest_error_body(body),
    })
}

/// Pure verdict fold: final Success → Green (even `payload_parse_failed`);
/// final Auth → Red; anything else → Degraded. RED is UNREACHABLE from any
/// GA code / envelope content alone (dedicated pin test).
pub fn verdict_for(final_attempt: &ProbeAttempt) -> ReadinessVerdict {
    match final_attempt {
        ProbeAttempt::Success { .. } => ReadinessVerdict::Green,
        ProbeAttempt::Failure {
            failure: ProbeFailure::Auth { .. },
            ..
        } => ReadinessVerdict::Red,
        ProbeAttempt::Failure { .. } => ReadinessVerdict::Degraded,
    }
}

/// Pure body-cap helper (the transport shell is thin glue around it):
/// declared > cap → `Err(Oversize{declared})`; accumulated ≥ cap →
/// `Err(Oversize{None})`.
pub fn clamp_probe_body(declared: Option<u64>, accumulated_len: usize) -> Result<(), ProbeFailure> {
    if let Some(declared_len) = declared
        && declared_len > READINESS_MAX_BODY_BYTES as u64
    {
        return Err(ProbeFailure::Oversize {
            declared_len: Some(declared_len),
        });
    }
    if accumulated_len >= READINESS_MAX_BODY_BYTES {
        return Err(ProbeFailure::Oversize { declared_len: None });
    }
    Ok(())
}

/// Drop non-finite / implausible rupee values (display-only defense).
fn sanitize_rupees(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite() && v.abs() <= MAX_PLAUSIBLE_RUPEES)
}

/// Fold a parsed payload into observations: nested-first / flattened-fallback
/// for the option-buy balance, headline-absence counting, implausible values
/// dropped + counted.
fn observations_from_payload(payload: MarginPayload) -> MarginObservations {
    let clear_cash = sanitize_rupees(payload.clear_cash);
    let headline = [
        clear_cash,
        sanitize_rupees(payload.net_margin_used),
        sanitize_rupees(payload.brokerage_and_charges),
        sanitize_rupees(payload.collateral_used),
        sanitize_rupees(payload.collateral_available),
        sanitize_rupees(payload.adhoc_margin),
    ];
    let absent = headline.iter().filter(|f| f.is_none()).count();
    let nested_option_buy = payload
        .fno_margin_details
        .as_ref()
        .and_then(|f| sanitize_rupees(f.option_buy_balance_available));
    let flattened_option_buy = sanitize_rupees(payload.option_buy_balance_available);
    let fno_margin = if payload.fno_margin_details.is_some() {
        FnoMarginObservation::Present
    } else {
        FnoMarginObservation::Absent
    };
    MarginObservations {
        clear_cash_rupees: clear_cash,
        option_buy_balance_rupees: nested_option_buy.or(flattened_option_buy),
        fno_margin,
        absent_headline_fields: u8::try_from(absent).unwrap_or(u8::MAX),
        payload_parse_failed: false,
        full_payload: payload,
    }
}

// ---------------------------------------------------------------------------
// §B (continued) — pure scheduling helpers (for the future wiring)
// ---------------------------------------------------------------------------

/// The once-per-day fire decision arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireDecision {
    /// All gates open — fire the probe now.
    Fire,
    /// The injected enable is false.
    Disabled,
    /// The caller's `TradingCalendar` says today is not a trading day.
    NotTradingDay,
    /// Before 09:15:30 IST.
    BeforeWindow,
    /// At/after 15:30:00 IST — never after close.
    AfterWindow,
    /// The day latch already claimed today.
    AlreadyFired,
}

/// One fire per trading day inside `[09:15:30, 15:30:00)` IST. The
/// `TradingCalendar` input is caller-owned (needs config); this is the
/// time-window + latch decision only. Total over `u32` secs-of-day.
pub fn readiness_fire_decision(
    enabled: bool,
    is_trading_day: bool,
    ist_secs_of_day: u32,
    already_fired_today: bool,
) -> FireDecision {
    if !enabled {
        return FireDecision::Disabled;
    }
    if !is_trading_day {
        return FireDecision::NotTradingDay;
    }
    if ist_secs_of_day < READINESS_FIRE_SECS_OF_DAY_IST {
        return FireDecision::BeforeWindow;
    }
    if ist_secs_of_day >= READINESS_FIRE_WINDOW_END_SECS_IST {
        return FireDecision::AfterWindow;
    }
    if already_fired_today {
        return FireDecision::AlreadyFired;
    }
    FireDecision::Fire
}

/// `(utc + 19_800).div_euclid(86_400)` — the IST day number; pre-1970-safe.
pub fn ist_day_number(utc_epoch_secs: i64) -> i64 {
    utc_epoch_secs
        .saturating_add(IST_UTC_OFFSET_SECONDS_I64)
        .div_euclid(86_400)
}

/// Monotonic once-per-day latch (an `AtomicI64` of [`ist_day_number`]).
/// `claim()` is a CAS refusing `day <= last-claimed` — a double spawn AND
/// backwards clock steps can never double-fire.
#[derive(Debug)]
pub struct ReadinessDayLatch(AtomicI64);

impl ReadinessDayLatch {
    /// A latch that has never fired.
    pub fn new() -> Self {
        Self(AtomicI64::new(i64::MIN))
    }

    /// Claim the fire for `ist_day`. Returns true for exactly one caller per
    /// day; refuses same-day repeats AND backwards days (clock-step defense).
    pub fn claim(&self, ist_day: i64) -> bool {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |last| {
                (ist_day > last).then_some(ist_day)
            })
            .is_ok()
    }
}

impl Default for ReadinessDayLatch {
    fn default() -> Self {
        Self::new()
    }
}

/// Pure pre-flight fold: `Disabled` outranks `OffHours`; `None` = proceed.
/// (Split out of the shell so the OffHours arm is deterministically testable
/// — the shared market-hours test hook can only force IN-market.)
fn readiness_refusal(enabled: bool, in_market_hours: bool) -> Option<ReadinessRefusal> {
    if !enabled {
        return Some(ReadinessRefusal::Disabled);
    }
    if !in_market_hours {
        return Some(ReadinessRefusal::OffHours);
    }
    None
}

/// Current IST seconds-of-day (freshness stamp input; cold path).
fn now_ist_secs_of_day() -> u32 {
    let ist_secs = chrono::Utc::now()
        .timestamp()
        .saturating_add(IST_UTC_OFFSET_SECONDS_I64);
    u32::try_from(ist_secs.rem_euclid(86_400)).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// §C — transport shell (the ONLY async/IO region)
// ---------------------------------------------------------------------------

/// Injectable timing so transport tests run sub-second; `Default` = the
/// module consts.
#[derive(Debug, Clone)]
pub struct ProbeTiming {
    /// Per-request bound (default 5s).
    pub request_timeout: Duration,
    /// The single bounded retry gap (default 300s).
    pub retry_delay: Duration,
}

impl Default for ProbeTiming {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(READINESS_REQUEST_TIMEOUT_SECS),
            retry_delay: Duration::from_secs(READINESS_RETRY_DELAY_SECS),
        }
    }
}

/// Builds (does NOT send) the GET with the doc-16 §1 mandatory header set:
/// `Authorization: Bearer <token>` (`set_sensitive(true)`) ·
/// `Accept: application/json` · `x-api-version: 1.0` (common consts) + the
/// per-request timeout. The secret-exposing call appears EXACTLY here and nowhere
/// else (source-scan pinned). The token never enters the URL; the built
/// `Request` is never Debug-formatted by this module.
pub fn probe_request(
    client: &reqwest::Client,
    base_url: &str,
    token: &SecretString,
    timing: &ProbeTiming,
) -> Result<reqwest::Request, ProbeFailure> {
    let url = format!("{base_url}{READINESS_MARGIN_ENDPOINT}");
    let mut authorization =
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.expose_secret()))
            .map_err(|_| ProbeFailure::Transport {
                // Deliberately a fixed string — the header error must never
                // echo token bytes.
                detail: "authorization header build refused the token shape".to_string(),
            })?;
    authorization.set_sensitive(true);
    client
        .get(url)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(GROWW_API_VERSION_HEADER, GROWW_API_VERSION_VALUE)
        .timeout(timing.request_timeout)
        .build()
        .map_err(|e| ProbeFailure::Transport {
            detail: capture_rest_error_body(&e.to_string()),
        })
}

/// Wall-clock ms since `started`, saturating.
fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// ONE attempt: build → send → cap-checked read (`clamp_probe_body`:
/// declared pre-check + accumulation cap; lossy-UTF8 → classifier) →
/// [`classify_response`]. No retry, no sleep, no emit.
pub async fn probe_once(
    client: &reqwest::Client,
    base_url: &str,
    token: &SecretString,
    timing: &ProbeTiming,
) -> ProbeAttempt {
    let started = std::time::Instant::now();
    let request = match probe_request(client, base_url, token, timing) {
        Ok(request) => request,
        Err(failure) => {
            return ProbeAttempt::Failure {
                failure,
                latency_ms: elapsed_ms(started),
            };
        }
    };
    let mut response = match client.execute(request).await {
        Ok(response) => response,
        Err(e) => {
            return ProbeAttempt::Failure {
                failure: ProbeFailure::Transport {
                    detail: capture_rest_error_body(&e.to_string()),
                },
                latency_ms: elapsed_ms(started),
            };
        }
    };
    let http_status = response.status().as_u16();
    let retry_after_secs = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    // Declared-length pre-check: an oversize body is refused before a single
    // body byte is read.
    if let Err(failure) = clamp_probe_body(response.content_length(), 0) {
        return ProbeAttempt::Failure {
            failure,
            latency_ms: elapsed_ms(started),
        };
    }
    let mut body_bytes: Vec<u8> = Vec::with_capacity(2048);
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                body_bytes.extend_from_slice(&chunk);
                // Accumulation cap: a chunked/undeclared hostile stream is
                // refused mid-read.
                if let Err(failure) = clamp_probe_body(None, body_bytes.len()) {
                    return ProbeAttempt::Failure {
                        failure,
                        latency_ms: elapsed_ms(started),
                    };
                }
            }
            Ok(None) => break,
            Err(e) => {
                return ProbeAttempt::Failure {
                    failure: ProbeFailure::Transport {
                        detail: capture_rest_error_body(&e.to_string()),
                    },
                    latency_ms: elapsed_ms(started),
                };
            }
        }
    }
    let body = String::from_utf8_lossy(&body_bytes);
    match classify_response(http_status, retry_after_secs, &body) {
        Ok(observations) => ProbeAttempt::Success {
            observations: Box::new(observations),
            latency_ms: elapsed_ms(started),
        },
        Err(failure) => ProbeAttempt::Failure {
            failure,
            latency_ms: elapsed_ms(started),
        },
    }
}

/// The single bounded retry gap: `min(retry_delay, Retry-After)` when the
/// server supplied one (already capped at 300s by the classifier).
fn retry_delay_for(failure: &ProbeFailure, timing: &ProbeTiming) -> Duration {
    match failure {
        ProbeFailure::RateLimited {
            retry_after_secs: Some(secs),
            ..
        } => Duration::from_secs(*secs).min(timing.retry_delay),
        _ => timing.retry_delay,
    }
}

/// The module's single entry point — the full bounded run:
/// 0. fail-closed pre-flight: `!enabled` → `Err(Disabled)`; outside market
///    hours → `Err(OffHours)`;
/// 1. `token = fetch_token()`; `None` → TokenUnavailable (stage `token_read`);
/// 2. attempt 1 = [`probe_once`];
/// 3. if failed AND retryable: sleep the bounded gap, `fetch_token()` AGAIN
///    (the caller's SSM re-read path — the ≥60s floor is trivially satisfied
///    by the 300s gap), attempt 2;
/// 4. fold the verdict, emit ONCE, increment counters, return the outcome.
///
/// GET is the only verb in the module. Never blocks/gates anything else; a
/// failed run loses one morning's signal only (fail-soft).
// WIRING-EXEMPT: code-ready surface — the scheduler spawn is the build-lead
// wiring PR (§39.3 Item 6; binding constraint 5).
pub async fn run_readiness_probe<F>(
    client: &reqwest::Client,
    base_url: &str,
    fetch_token: F,
    enabled: bool,
    timing: &ProbeTiming,
) -> Result<ReadinessOutcome, ReadinessRefusal>
where
    F: Fn() -> Option<SecretString>,
{
    if let Some(refusal) = readiness_refusal(enabled, is_within_market_hours_ist()) {
        match refusal {
            ReadinessRefusal::Disabled => {
                // Expected daily state while the order side ships dark.
                debug!(
                    feed = READINESS_FEED,
                    leg = READINESS_LEG,
                    "groww readiness probe disabled — refusing before any request"
                );
                metrics::counter!("tv_groww_readiness_refused_total", "reason" => "disabled")
                    .increment(1);
            }
            ReadinessRefusal::OffHours => {
                // A mis-wired scheduler is a bug worth seeing (no ErrorCode —
                // the refusal IS the gate working).
                warn!(
                    feed = READINESS_FEED,
                    leg = READINESS_LEG,
                    "groww readiness probe refused outside market hours — check the scheduler"
                );
                metrics::counter!("tv_groww_readiness_refused_total", "reason" => "off_hours")
                    .increment(1);
            }
        }
        return Err(refusal);
    }
    let fired_secs_of_day_ist = now_ist_secs_of_day();
    let mut attempts: u8 = 0;
    let final_attempt = loop {
        attempts = attempts.saturating_add(1);
        let attempt = match fetch_token() {
            Some(token) => probe_once(client, base_url, &token, timing).await,
            None => ProbeAttempt::Failure {
                failure: ProbeFailure::TokenUnavailable,
                latency_ms: 0,
            },
        };
        let retry = attempts < READINESS_MAX_ATTEMPTS
            && matches!(
                &attempt,
                ProbeAttempt::Failure { failure, .. } if failure.is_retryable()
            );
        if !retry {
            break attempt;
        }
        if let ProbeAttempt::Failure { failure, .. } = &attempt {
            metrics::counter!("tv_groww_readiness_retries_total").increment(1);
            tokio::time::sleep(retry_delay_for(failure, timing)).await;
        }
    };
    let verdict = verdict_for(&final_attempt);
    let (observations, failure, last_latency_ms) = match final_attempt {
        ProbeAttempt::Success {
            observations,
            latency_ms,
        } => (Some(*observations), None, latency_ms),
        ProbeAttempt::Failure {
            failure,
            latency_ms,
        } => (None, Some(failure), latency_ms),
    };
    let outcome = ReadinessOutcome {
        verdict,
        observations,
        failure,
        attempts,
        last_latency_ms,
        fired_secs_of_day_ist,
    };
    emit_outcome(&outcome);
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// §D — emit + counters (private helpers)
// ---------------------------------------------------------------------------

/// ONE coalesced emit per run (edge discipline, audit Rule 4). Reuses
/// `SPOT1M-01` with `leg="readiness"` — zero new ErrorCode variants (§39.3).
fn emit_outcome(outcome: &ReadinessOutcome) {
    metrics::counter!(
        "tv_groww_readiness_probe_total",
        "outcome" => outcome.verdict.as_str()
    )
    .increment(1);
    match &outcome.failure {
        Some(failure) => {
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = failure.stage(),
                feed = READINESS_FEED,
                leg = READINESS_LEG,
                verdict = outcome.verdict.as_str(),
                attempts = outcome.attempts,
                http_status = failure.http_status_or_zero(),
                ga_code = failure.ga_code_str(),
                detail = failure.detail_str(),
                "SPOT1M-01: groww readiness probe degraded — one morning's signal lost, \
                 nothing gated"
            );
        }
        None if outcome
            .observations
            .as_ref()
            .is_some_and(|o| o.payload_parse_failed) =>
        {
            warn!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "payload_parse",
                feed = READINESS_FEED,
                leg = READINESS_LEG,
                verdict = outcome.verdict.as_str(),
                attempts = outcome.attempts,
                "SPOT1M-01: groww readiness GREEN with an unusable funds payload — the \
                 login is proven, the observation is degraded"
            );
        }
        None => {
            info!(
                feed = READINESS_FEED,
                leg = READINESS_LEG,
                verdict = outcome.verdict.as_str(),
                attempts = outcome.attempts,
                latency_ms = outcome.last_latency_ms,
                "groww readiness probe GREEN — order login confirmed (login only; feed \
                 entitlement not implied)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex, MutexGuard};

    use proptest::prelude::*;
    use tickvault_common::market_hours::set_test_force_in_market_hours;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    // -- fixtures -----------------------------------------------------------

    /// Doc-16 §3.1 nested-shape sample (captured values `5000` class — the
    /// refuted `96.21` research sample never appears in this module).
    const NESTED_SUCCESS_BODY: &str = r#"{
        "status": "SUCCESS",
        "payload": {
            "clear_cash": 5000.0,
            "net_margin_used": 100.0,
            "brokerage_and_charges": 10.0,
            "collateral_used": 0.0,
            "collateral_available": 200.0,
            "adhoc_margin": 0.0,
            "fno_margin_details": {
                "net_fno_margin_used": 50.0,
                "span_margin_used": 30.0,
                "exposure_margin_used": 20.0,
                "future_balance_available": 1000.0,
                "option_buy_balance_available": 800.0,
                "option_sell_balance_available": 700.0
            },
            "equity_margin_details": {
                "net_equity_margin_used": 5.0,
                "cnc_margin_used": 1.0,
                "mis_margin_used": 2.0,
                "cnc_balance_available": 3.0,
                "mis_balance_available": 4.0
            },
            "commodity_margin_details": {
                "commodity_span_margin": 0.0,
                "commodity_exposure_margin": 0.0,
                "commodity_tender_margin": 0.0,
                "commodity_special_margin": 0.0,
                "commodity_additional_margin": 0.0,
                "commodity_unrealised_m2m": 0.0,
                "commodity_realised_m2m": 0.0
            }
        }
    }"#;

    /// The doc-table flattened shape: fields beside `status`, no `payload`.
    const FLATTENED_SUCCESS_BODY: &str = r#"{
        "status": "SUCCESS",
        "clear_cash": 5000.0,
        "net_margin_used": 100.0,
        "brokerage_and_charges": 10.0,
        "collateral_used": 0.0,
        "collateral_available": 200.0,
        "adhoc_margin": 0.0,
        "option_buy_balance_available": 800.0
    }"#;

    /// Doc-16 §5 failure envelope, verbatim.
    const GA001_FAILURE_BODY: &str = r#"{
        "status": "FAILURE",
        "error": {
            "code": "GA001",
            "message": "Invalid trading symbol.",
            "metadata": null
        }
    }"#;

    const GA005_FAILURE_BODY: &str = r#"{
        "status": "FAILURE",
        "error": {
            "code": "GA005",
            "message": "User not authorised to perform this operation",
            "metadata": null
        }
    }"#;

    fn success_outcome(fired: u32) -> ReadinessOutcome {
        let observations =
            classify_response(200, None, NESTED_SUCCESS_BODY).expect("fixture must classify Ok");
        ReadinessOutcome {
            verdict: ReadinessVerdict::Green,
            observations: Some(observations),
            failure: None,
            attempts: 1,
            last_latency_ms: 12,
            fired_secs_of_day_ist: fired,
        }
    }

    fn failure_outcome(failure: ProbeFailure) -> ReadinessOutcome {
        let verdict = verdict_for(&ProbeAttempt::Failure {
            failure: failure.clone(),
            latency_ms: 7,
        });
        ReadinessOutcome {
            verdict,
            observations: None,
            failure: Some(failure),
            attempts: 2,
            last_latency_ms: 7,
            fired_secs_of_day_ist: READINESS_FIRE_SECS_OF_DAY_IST,
        }
    }

    // -- glue helpers -------------------------------------------------------

    /// Serializes tests that force the process-global market-hours override,
    /// resetting it on drop (the house activity-watchdog pattern).
    static MARKET_HOURS_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct ForcedMarketHours(#[allow(dead_code)] MutexGuard<'static, ()>);

    impl ForcedMarketHours {
        fn on() -> Self {
            let guard = MARKET_HOURS_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            set_test_force_in_market_hours(true);
            Self(guard)
        }
    }

    impl Drop for ForcedMarketHours {
        fn drop(&mut self) {
            set_test_force_in_market_hours(false);
        }
    }

    fn http_response(status_line: &str, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{body}",
            body.len()
        )
    }

    /// One-shot localhost responder: serves the canned responses in order
    /// (one connection each) and captures every request's head bytes.
    async fn spawn_one_shot_server(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("test listener addr");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 8192];
                let mut head = Vec::new();
                loop {
                    let Ok(n) = stream.read(&mut buf).await else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    head.extend_from_slice(&buf[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                sink.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(String::from_utf8_lossy(&head).to_string());
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (format!("http://{addr}"), captured)
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .build()
            .expect("test client build")
    }

    fn fast_timing() -> ProbeTiming {
        ProbeTiming {
            request_timeout: Duration::from_secs(5),
            retry_delay: Duration::from_millis(5),
        }
    }

    // -- pure classifier ----------------------------------------------------

    #[test]
    fn test_classify_200_success_full_payload_green_observations() {
        let obs = classify_response(200, None, NESTED_SUCCESS_BODY).expect("must be Ok");
        assert_eq!(obs.clear_cash_rupees, Some(5000.0));
        assert_eq!(obs.option_buy_balance_rupees, Some(800.0));
        assert_eq!(obs.fno_margin, FnoMarginObservation::Present);
        assert_eq!(obs.absent_headline_fields, 0);
        assert!(!obs.payload_parse_failed);
        assert_eq!(
            obs.full_payload
                .equity_margin_details
                .as_ref()
                .and_then(|e| e.mis_balance_available),
            Some(4.0)
        );
    }

    #[test]
    fn test_classify_200_success_missing_fields_counts_absent_never_fails() {
        let body = r#"{"status":"SUCCESS","payload":{"clear_cash":5000.0}}"#;
        let obs = classify_response(200, None, body).expect("absence must degrade, never fail");
        assert_eq!(obs.clear_cash_rupees, Some(5000.0));
        assert_eq!(obs.absent_headline_fields, 5);
        assert_eq!(obs.fno_margin, FnoMarginObservation::Absent);
        assert_eq!(obs.option_buy_balance_rupees, None);
        assert!(!obs.payload_parse_failed);
        // Implausible values are dropped + counted absent.
        let hostile = r#"{"status":"SUCCESS","payload":{"clear_cash":1e300}}"#;
        let obs = classify_response(200, None, hostile).expect("must be Ok");
        assert_eq!(obs.clear_cash_rupees, None);
        assert_eq!(obs.absent_headline_fields, 6);
    }

    #[test]
    fn test_classify_200_success_flattened_payload_fallback() {
        let obs = classify_response(200, None, FLATTENED_SUCCESS_BODY)
            .expect("flattened doc-table shape must parse");
        assert_eq!(obs.clear_cash_rupees, Some(5000.0));
        assert_eq!(obs.option_buy_balance_rupees, Some(800.0));
        assert_eq!(obs.fno_margin, FnoMarginObservation::Absent);
        assert_eq!(obs.absent_headline_fields, 0);
        // Bare payload object (no status key at all) — envelope-drift defense.
        let bare = r#"{"clear_cash":5000.0,"adhoc_margin":1.0}"#;
        let obs = classify_response(200, None, bare).expect("bare payload must parse");
        assert_eq!(obs.clear_cash_rupees, Some(5000.0));
        // Nested wins over flattened when both are present.
        let both = r#"{"status":"SUCCESS","payload":{
            "fno_margin_details":{"option_buy_balance_available":1.0},
            "option_buy_balance_available":2.0}}"#;
        let obs = classify_response(200, None, both).expect("must be Ok");
        assert_eq!(obs.option_buy_balance_rupees, Some(1.0));
    }

    #[test]
    fn test_classify_200_success_garbage_payload_green_payload_parse_flag() {
        // F9: envelope SUCCESS + wholly-unusable payload → STILL Ok (the
        // envelope proves the token), flag set, fno = PayloadUnparsed.
        let body = r#"{"status":"SUCCESS","payload":{"clear_cash":"not-a-number"}}"#;
        let obs = classify_response(200, None, body).expect("SUCCESS envelope must stay Ok");
        assert!(obs.payload_parse_failed);
        assert_eq!(obs.fno_margin, FnoMarginObservation::PayloadUnparsed);
        assert_eq!(obs.absent_headline_fields, HEADLINE_FIELD_COUNT);
        assert_eq!(obs.clear_cash_rupees, None);
    }

    #[test]
    fn test_classify_200_failure_envelope_wins_over_http() {
        let err = classify_response(200, None, GA001_FAILURE_BODY).expect_err("envelope wins");
        match &err {
            ProbeFailure::VendorReject {
                http_status,
                ga_code,
                body_sample,
            } => {
                assert_eq!(*http_status, 200);
                assert_eq!(ga_code, "GA001");
                assert!(body_sample.contains("Invalid trading symbol"));
            }
            other => panic!("expected VendorReject, got {other:?}"),
        }
        assert!(
            !err.is_retryable(),
            "vendor rejects are never blind-retried"
        );
    }

    #[test]
    fn test_classify_2xx_failure_ga005_is_vendor_reject_not_auth() {
        // The G1 pin: RED (auth) is unreachable from body content alone.
        let err = classify_response(200, None, GA005_FAILURE_BODY).expect_err("must fail");
        assert!(matches!(&err, ProbeFailure::VendorReject { ga_code, .. } if ga_code == "GA005"));
        assert!(!err.requires_token_reread());
        let attempt = ProbeAttempt::Failure {
            failure: err,
            latency_ms: 1,
        };
        assert_eq!(verdict_for(&attempt), ReadinessVerdict::Degraded);
    }

    #[test]
    fn test_classify_401_is_auth_requires_token_reread() {
        let err = classify_response(401, None, "").expect_err("401 must be auth");
        match &err {
            ProbeFailure::Auth {
                http_status,
                ga_code,
                ..
            } => {
                assert_eq!(*http_status, 401);
                assert_eq!(ga_code, "none");
            }
            other => panic!("expected Auth, got {other:?}"),
        }
        assert!(err.requires_token_reread());
        assert!(
            err.is_retryable(),
            "auth retries once with a re-fetched token"
        );
    }

    #[test]
    fn test_classify_403_with_ga005_is_auth_ga_code_captured() {
        let err = classify_response(403, None, GA005_FAILURE_BODY).expect_err("403 must be auth");
        match &err {
            ProbeFailure::Auth {
                http_status,
                ga_code,
                body_sample,
            } => {
                assert_eq!(*http_status, 403);
                assert_eq!(ga_code, "GA005");
                assert!(body_sample.contains("not authorised"));
            }
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_401_with_success_body_is_still_auth() {
        // F11 contradiction pin: a broken proxy replaying a SUCCESS body on a
        // 401 must not read GREEN — the HTTP status wins.
        let err = classify_response(401, None, NESTED_SUCCESS_BODY).expect_err("status must win");
        assert!(matches!(
            err,
            ProbeFailure::Auth {
                http_status: 401,
                ..
            }
        ));
    }

    #[test]
    fn test_classify_429_rate_limited_retryable_retry_after_captured_and_capped() {
        let err = classify_response(429, Some(30), "").expect_err("429 must fail");
        assert!(matches!(
            err,
            ProbeFailure::RateLimited {
                http_status: 429,
                retry_after_secs: Some(30),
            }
        ));
        assert!(err.is_retryable());
        // Cap: a hostile Retry-After can never stretch past 300s.
        let err = classify_response(429, Some(86_400), "").expect_err("429 must fail");
        assert!(matches!(
            err,
            ProbeFailure::RateLimited {
                retry_after_secs: Some(READINESS_RETRY_DELAY_SECS),
                ..
            }
        ));
        let err = classify_response(429, None, "").expect_err("429 must fail");
        assert!(matches!(
            err,
            ProbeFailure::RateLimited {
                retry_after_secs: None,
                ..
            }
        ));
    }

    #[test]
    fn test_classify_400_and_404_http_error_not_retryable() {
        for status in [400_u16, 404, 418] {
            let err = classify_response(status, None, "nope").expect_err("must fail");
            assert!(
                matches!(err, ProbeFailure::HttpError { http_status, .. } if http_status == status)
            );
            assert!(!err.is_retryable(), "{status} must not retry");
        }
    }

    #[test]
    fn test_classify_500_and_504_http_error_retryable() {
        for status in [500_u16, 504] {
            let err = classify_response(status, None, "oops").expect_err("must fail");
            assert!(
                matches!(err, ProbeFailure::HttpError { http_status, .. } if http_status == status)
            );
            assert!(err.is_retryable(), "{status} must retry (server class)");
        }
    }

    #[test]
    fn test_classify_200_non_json_body_is_parse_failure() {
        let err = classify_response(200, None, "<html>gateway</html>").expect_err("must fail");
        assert!(matches!(
            err,
            ProbeFailure::Parse {
                http_status: 200,
                ..
            }
        ));
        assert!(err.is_retryable());
        // Non-object JSON + unrecognized envelope statuses land Parse too.
        let err = classify_response(200, None, "[1,2,3]").expect_err("must fail");
        assert!(matches!(err, ProbeFailure::Parse { .. }));
        let err = classify_response(200, None, r#"{"status":"PENDING"}"#).expect_err("must fail");
        assert!(matches!(err, ProbeFailure::Parse { .. }));
    }

    #[test]
    fn test_clamp_probe_body_declared_and_accumulated_caps() {
        assert!(clamp_probe_body(None, 0).is_ok());
        assert!(clamp_probe_body(Some(READINESS_MAX_BODY_BYTES as u64), 0).is_ok());
        let err = clamp_probe_body(Some(READINESS_MAX_BODY_BYTES as u64 + 1), 0)
            .expect_err("declared over cap");
        assert!(matches!(
            err,
            ProbeFailure::Oversize {
                declared_len: Some(_)
            }
        ));
        assert!(clamp_probe_body(None, READINESS_MAX_BODY_BYTES - 1).is_ok());
        let err = clamp_probe_body(None, READINESS_MAX_BODY_BYTES).expect_err("accumulated at cap");
        assert!(matches!(err, ProbeFailure::Oversize { declared_len: None }));
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_body_samples_are_redacted_and_capped() {
        let jwt = format!(
            "eyJ{}.eyJ{}.sig{}",
            "a".repeat(40),
            "b".repeat(40),
            "c".repeat(40)
        );
        let body = format!(
            r#"{{"detail":"boom {jwt}","access_token":"{jwt}","pad":"{}"}}"#,
            "x".repeat(600)
        );
        let err = classify_response(502, None, &body).expect_err("must fail");
        let sample = err.detail_str();
        assert!(
            !sample.contains("eyJ"),
            "JWT shape must be redacted: {sample}"
        );
        assert!(
            sample.len() <= 320,
            "sample must be bounded: {}",
            sample.len()
        );
    }

    #[test]
    fn test_ga_sanitizer_vectors_match_1539() {
        // Copy-drift pins vs `groww_spot_1m_boot.rs::parse_groww_ga_failure`.
        let ga = sniff_ga_failure(GA005_FAILURE_BODY).expect("must sniff");
        assert_eq!(ga.ga_code, "GA005");
        let ga = sniff_ga_failure(r#"{"status":"FAILURE"}"#).expect("must sniff");
        assert_eq!(ga.ga_code, "none");
        let ga =
            sniff_ga_failure(r#"{"status":"FAILURE","error":{"code":""}}"#).expect("must sniff");
        assert_eq!(ga.ga_code, "none");
        // Hostile code: control/quote chars filtered, bounded at 16.
        let hostile = format!(
            r#"{{"status":"FAILURE","error":{{"code":"GA\n005\u001b[31m'{}"}}}}"#,
            "Z".repeat(40)
        );
        let ga = sniff_ga_failure(&hostile).expect("hostile FAILURE must still sniff");
        assert!(ga.ga_code.len() <= GA_CODE_MAX_CHARS);
        assert!(
            ga.ga_code
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
        // SUCCESS / non-JSON → None.
        assert!(sniff_ga_failure(NESTED_SUCCESS_BODY).is_none());
        assert!(sniff_ga_failure("not json").is_none());
        // The message sample is PRE-redacted here (unlike the app-side raw
        // message contract).
        let jwt_msg = format!(
            r#"{{"status":"FAILURE","error":{{"code":"GA000","message":"eyJ{}.eyJ{}.s{}"}}}}"#,
            "a".repeat(40),
            "b".repeat(40),
            "c".repeat(40)
        );
        let ga = sniff_ga_failure(&jwt_msg).expect("must sniff");
        assert!(!ga.message_sample.contains("eyJ"));
    }

    #[test]
    fn test_stage_slug_totality_and_stability() {
        let all = [
            (
                ProbeFailure::Auth {
                    http_status: 401,
                    ga_code: "none".into(),
                    body_sample: String::new(),
                },
                "auth",
            ),
            (
                ProbeFailure::RateLimited {
                    http_status: 429,
                    retry_after_secs: None,
                },
                "rate_limited",
            ),
            (
                ProbeFailure::VendorReject {
                    http_status: 200,
                    ga_code: "GA001".into(),
                    body_sample: String::new(),
                },
                "vendor_reject",
            ),
            (
                ProbeFailure::HttpError {
                    http_status: 500,
                    body_sample: String::new(),
                },
                "http_error",
            ),
            (
                ProbeFailure::Parse {
                    http_status: 200,
                    detail: String::new(),
                },
                "parse",
            ),
            (ProbeFailure::Oversize { declared_len: None }, "oversize"),
            (
                ProbeFailure::Transport {
                    detail: String::new(),
                },
                "transport",
            ),
            (ProbeFailure::TokenUnavailable, "token_read"),
        ];
        assert_eq!(all.len(), 8, "the slug set is closed at 8");
        for (failure, slug) in &all {
            assert_eq!(failure.stage(), *slug);
        }
    }

    #[test]
    fn test_verdict_fold_green_red_degraded() {
        let green = ProbeAttempt::Success {
            observations: Box::new(MarginObservations {
                payload_parse_failed: true,
                ..MarginObservations::default()
            }),
            latency_ms: 1,
        };
        assert_eq!(
            verdict_for(&green),
            ReadinessVerdict::Green,
            "payload_parse_failed stays GREEN — the envelope proved the token"
        );
        // RED ⇔ Auth, exhaustively: every non-auth failure folds Degraded.
        let auth = ProbeFailure::Auth {
            http_status: 403,
            ga_code: "none".into(),
            body_sample: String::new(),
        };
        assert_eq!(
            verdict_for(&ProbeAttempt::Failure {
                failure: auth,
                latency_ms: 1
            }),
            ReadinessVerdict::Red
        );
        for failure in [
            ProbeFailure::RateLimited {
                http_status: 429,
                retry_after_secs: None,
            },
            ProbeFailure::VendorReject {
                http_status: 200,
                ga_code: "GA005".into(),
                body_sample: String::new(),
            },
            ProbeFailure::HttpError {
                http_status: 500,
                body_sample: String::new(),
            },
            ProbeFailure::Parse {
                http_status: 200,
                detail: String::new(),
            },
            ProbeFailure::Oversize { declared_len: None },
            ProbeFailure::Transport {
                detail: String::new(),
            },
            ProbeFailure::TokenUnavailable,
        ] {
            assert_eq!(
                verdict_for(&ProbeAttempt::Failure {
                    failure,
                    latency_ms: 1
                }),
                ReadinessVerdict::Degraded,
                "RED must be unreachable from anything but HTTP-proven auth"
            );
        }
        assert_eq!(ReadinessVerdict::Green.as_str(), "green");
        assert_eq!(ReadinessVerdict::Red.as_str(), "red");
        assert_eq!(ReadinessVerdict::Degraded.as_str(), "degraded");
        assert_eq!(FnoMarginObservation::Present.as_str(), "present");
        assert_eq!(FnoMarginObservation::Absent.as_str(), "absent");
        assert_eq!(
            FnoMarginObservation::PayloadUnparsed.as_str(),
            "payload_unparsed"
        );
    }

    #[test]
    fn test_readiness_feed_leg_literals_pinned() {
        assert_eq!(READINESS_FEED, "groww");
        assert_eq!(READINESS_LEG, "readiness");
        assert_eq!(READINESS_AUDIT_SYMBOL, "READINESS");
        assert_eq!(READINESS_DEFAULT_BASE_URL, "https://api.groww.in");
    }

    proptest! {
        #[test]
        fn prop_classify_response_never_panics(
            status in any::<u16>(),
            retry_after in any::<Option<u64>>(),
            body in ".*",
        ) {
            let _ = classify_response(status, retry_after, &body);
        }

        #[test]
        fn prop_sniff_ga_failure_total(body in ".*") {
            let _ = sniff_ga_failure(&body);
        }
    }

    // -- scheduling ---------------------------------------------------------

    #[test]
    fn test_fire_decision_boundaries() {
        let fire = READINESS_FIRE_SECS_OF_DAY_IST;
        let end = READINESS_FIRE_WINDOW_END_SECS_IST;
        assert_eq!(fire, 33_330);
        assert_eq!(end, 55_800);
        assert_eq!(
            readiness_fire_decision(false, true, fire, false),
            FireDecision::Disabled
        );
        assert_eq!(
            readiness_fire_decision(true, false, fire, false),
            FireDecision::NotTradingDay
        );
        assert_eq!(
            readiness_fire_decision(true, true, fire - 1, false),
            FireDecision::BeforeWindow
        );
        assert_eq!(
            readiness_fire_decision(true, true, fire, false),
            FireDecision::Fire
        );
        assert_eq!(
            readiness_fire_decision(true, true, end - 1, false),
            FireDecision::Fire
        );
        assert_eq!(
            readiness_fire_decision(true, true, end, false),
            FireDecision::AfterWindow
        );
        assert_eq!(
            readiness_fire_decision(true, true, fire, true),
            FireDecision::AlreadyFired
        );
    }

    #[test]
    fn test_fire_const_is_market_hours_compliant() {
        // The §10 grant conditions read-only GETs on market-hours-only. A
        // silent flip to the 08:50 IST pre-open slot fails HERE — the flip
        // requires a fresh dated §39 amendment FIRST, then this assertion
        // moves with it.
        let market_open = 9 * 3600 + 15 * 60;
        let market_close = 15 * 3600 + 30 * 60;
        assert!(READINESS_FIRE_SECS_OF_DAY_IST >= market_open);
        assert!(READINESS_FIRE_SECS_OF_DAY_IST < market_close);
        assert!(READINESS_FIRE_WINDOW_END_SECS_IST <= market_close);
        assert!(
            READINESS_RETRY_DELAY_SECS >= 60,
            "≥ the token re-read floor"
        );
    }

    #[test]
    fn test_ist_day_number_boundaries() {
        assert_eq!(ist_day_number(0), 0);
        assert_eq!(ist_day_number(86_400 - 19_800 - 1), 0);
        assert_eq!(ist_day_number(86_400 - 19_800), 1);
        assert_eq!(ist_day_number(-19_800), 0);
        assert_eq!(
            ist_day_number(-19_801),
            -1,
            "pre-1970 must floor, not round"
        );
    }

    #[test]
    fn test_day_latch_monotonic_and_concurrent() {
        let latch = ReadinessDayLatch::new();
        assert!(latch.claim(20_000));
        assert!(!latch.claim(20_000), "same-day double claim refused");
        assert!(!latch.claim(19_999), "BACKWARDS day refused (clock step)");
        assert!(latch.claim(20_001), "next day claims");
        let latch = ReadinessDayLatch::default();
        let winners = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    if latch.claim(20_650) {
                        winners.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(winners.load(Ordering::SeqCst), 1, "exactly one race winner");
    }

    #[test]
    fn test_readiness_refusal_pure_arms() {
        // OffHours is deterministically testable only here: the shared
        // market-hours test hook can force IN-market, never out.
        assert_eq!(
            readiness_refusal(false, true),
            Some(ReadinessRefusal::Disabled)
        );
        assert_eq!(
            readiness_refusal(false, false),
            Some(ReadinessRefusal::Disabled),
            "Disabled outranks OffHours"
        );
        assert_eq!(
            readiness_refusal(true, false),
            Some(ReadinessRefusal::OffHours)
        );
        assert_eq!(readiness_refusal(true, true), None);
    }

    // -- glue (localhost fixtures via the injected base_url; no live network)

    #[tokio::test]
    async fn test_probe_request_shape() {
        let client = test_client();
        let timing = ProbeTiming::default();
        let token = SecretString::from("tok-secret-123");
        let request = probe_request(&client, "http://127.0.0.1:1", &token, &timing)
            .expect("build must succeed");
        assert_eq!(request.method(), reqwest::Method::GET);
        assert_eq!(
            request.url().as_str(),
            format!("http://127.0.0.1:1{READINESS_MARGIN_ENDPOINT}")
        );
        assert!(
            !request.url().as_str().contains("tok-secret"),
            "token must never enter the URL"
        );
        let auth = request
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .expect("authorization header present");
        assert!(auth.is_sensitive(), "authorization must be set_sensitive");
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::ACCEPT)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        assert_eq!(
            request
                .headers()
                .get(GROWW_API_VERSION_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some(GROWW_API_VERSION_VALUE)
        );
        assert_eq!(request.timeout(), Some(&timing.request_timeout));
    }

    #[tokio::test]
    async fn test_run_refuses_disabled_zero_http() {
        // The unroutable base URL proves no request is even attempted: a send
        // would surface as Ok(Degraded/transport), not Err(Disabled).
        let client = test_client();
        let result = run_readiness_probe(
            &client,
            "http://127.0.0.1:9",
            || Some(SecretString::from("t")),
            false,
            &fast_timing(),
        )
        .await;
        assert_eq!(result, Err(ReadinessRefusal::Disabled));
    }

    #[tokio::test]
    async fn test_run_token_unavailable_degraded_stage_token_read() {
        let _hours = ForcedMarketHours::on();
        let client = test_client();
        let outcome =
            run_readiness_probe(&client, "http://127.0.0.1:9", || None, true, &fast_timing())
                .await
                .expect("token-unavailable is an outcome, not a refusal");
        assert_eq!(outcome.verdict, ReadinessVerdict::Degraded);
        assert_eq!(outcome.audit_outcome_slug(), "token_read");
        assert_eq!(
            outcome.attempts, 2,
            "token_read is retryable — closure re-called"
        );
        assert!(!outcome.requires_token_reread());
    }

    #[tokio::test]
    async fn test_run_retries_transient_once_then_success_green() {
        let _hours = ForcedMarketHours::on();
        let responses = vec![
            http_response("500 Internal Server Error", "", "oops"),
            http_response("200 OK", "", NESTED_SUCCESS_BODY),
        ];
        let (base_url, _captured) = spawn_one_shot_server(responses).await;
        let client = test_client();
        let outcome = run_readiness_probe(
            &client,
            &base_url,
            || Some(SecretString::from("tok")),
            true,
            &fast_timing(),
        )
        .await
        .expect("must produce an outcome");
        assert_eq!(outcome.verdict, ReadinessVerdict::Green);
        assert_eq!(outcome.attempts, 2);
        assert_eq!(outcome.audit_outcome_slug(), "ok");
        assert_eq!(
            outcome
                .observations
                .as_ref()
                .and_then(|o| o.clear_cash_rupees),
            Some(5000.0)
        );
    }

    #[tokio::test]
    async fn test_run_auth_refetches_token_on_retry() {
        let _hours = ForcedMarketHours::on();
        let responses = vec![
            http_response("401 Unauthorized", "", ""),
            http_response("401 Unauthorized", "", ""),
        ];
        let (base_url, captured) = spawn_one_shot_server(responses).await;
        let client = test_client();
        let calls = AtomicUsize::new(0);
        let outcome = run_readiness_probe(
            &client,
            &base_url,
            || {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                Some(SecretString::from(format!("token-{n}")))
            },
            true,
            &fast_timing(),
        )
        .await
        .expect("must produce an outcome");
        assert_eq!(outcome.verdict, ReadinessVerdict::Red);
        assert_eq!(outcome.attempts, 2);
        assert!(outcome.requires_token_reread());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "fetch_token re-invoked on retry"
        );
        let requests = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("Bearer token-1"));
        assert!(
            requests[1].contains("Bearer token-2"),
            "the retry must carry the re-fetched token"
        );
    }

    #[tokio::test]
    async fn test_run_attempts_capped_at_two() {
        let _hours = ForcedMarketHours::on();
        let responses = vec![
            http_response("500 Internal Server Error", "", "a"),
            http_response("500 Internal Server Error", "", "b"),
            http_response("200 OK", "", NESTED_SUCCESS_BODY),
        ];
        let (base_url, captured) = spawn_one_shot_server(responses).await;
        let client = test_client();
        let outcome = run_readiness_probe(
            &client,
            &base_url,
            || Some(SecretString::from("tok")),
            true,
            &fast_timing(),
        )
        .await
        .expect("must produce an outcome");
        assert_eq!(outcome.attempts, 2, "never a third attempt");
        assert_eq!(outcome.verdict, ReadinessVerdict::Degraded);
        assert_eq!(outcome.audit_outcome_slug(), "http_error");
        assert_eq!(
            captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn test_run_vendor_reject_no_retry_attempts_one() {
        let _hours = ForcedMarketHours::on();
        let responses = vec![http_response("200 OK", "", GA001_FAILURE_BODY)];
        let (base_url, captured) = spawn_one_shot_server(responses).await;
        let client = test_client();
        let outcome = run_readiness_probe(
            &client,
            &base_url,
            || Some(SecretString::from("tok")),
            true,
            &fast_timing(),
        )
        .await
        .expect("must produce an outcome");
        assert_eq!(outcome.attempts, 1, "VAL discipline — never blind-retried");
        assert_eq!(outcome.verdict, ReadinessVerdict::Degraded);
        assert_eq!(outcome.audit_outcome_slug(), "vendor_reject");
        assert_eq!(
            captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn test_run_oversize_declared_content_length_refused_pre_read() {
        let _hours = ForcedMarketHours::on();
        // Headers only, huge declared length, no body — the probe must refuse
        // on the declaration without reading a byte.
        let response =
            "HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n".to_string();
        let (base_url, _captured) = spawn_one_shot_server(vec![response]).await;
        let client = test_client();
        let outcome = run_readiness_probe(
            &client,
            &base_url,
            || Some(SecretString::from("tok")),
            true,
            &fast_timing(),
        )
        .await
        .expect("must produce an outcome");
        assert_eq!(outcome.attempts, 1, "oversize is not retryable");
        assert_eq!(outcome.audit_outcome_slug(), "oversize");
        assert!(matches!(
            outcome.failure,
            Some(ProbeFailure::Oversize {
                declared_len: Some(100_000)
            })
        ));
    }

    // -- outcome surface ----------------------------------------------------

    #[test]
    fn test_operator_summary_wording() {
        let green = success_outcome(READINESS_FIRE_SECS_OF_DAY_IST).operator_summary();
        assert!(green.starts_with("🟢"));
        assert!(green.contains("confirms the LOGIN only"));
        assert!(green.contains("9:15 AM IST"));
        assert!(green.contains("5000.00"));
        let red = failure_outcome(ProbeFailure::Auth {
            http_status: 401,
            ga_code: "none".into(),
            body_sample: String::new(),
        })
        .operator_summary();
        assert!(red.starts_with("🆘"));
        assert!(red.contains("6:05"), "must name the morning key machine");
        assert!(red.contains("subscription"));
        assert!(!red.contains("hints"), "no GA005 → no subscription hint");
        let red_hint = failure_outcome(ProbeFailure::Auth {
            http_status: 403,
            ga_code: "GA005".into(),
            body_sample: String::new(),
        })
        .operator_summary();
        assert!(
            red_hint.contains("best guess"),
            "GA005 decorates a HINT only"
        );
        let degraded = failure_outcome(ProbeFailure::Transport {
            detail: "dns".into(),
        })
        .operator_summary();
        assert!(degraded.starts_with("⚠️"));
        assert!(degraded.contains("live price feed is unaffected"));
        // The 10-commandments sweep: no paths, no library names, no refuted
        // research sample values.
        for body in [&green, &red, &red_hint, &degraded] {
            assert!(!body.contains("96.21"), "refuted sample value banned");
            assert!(!body.contains("crates/"), "no file paths");
            assert!(!body.contains(".rs"), "no file paths");
            assert!(!body.contains("reqwest"), "no library names");
            assert!(!body.contains("Lambda"), "no infra jargon");
            assert!(!body.contains("SSM"), "no infra jargon");
        }
        assert_eq!(format_ist_12h(0), "12:00 AM IST");
        assert_eq!(format_ist_12h(12 * 3600), "12:00 PM IST");
        assert_eq!(format_ist_12h(15 * 3600 + 29 * 60), "3:29 PM IST");
    }

    #[test]
    fn test_audit_outcome_slugs_stable() {
        assert_eq!(success_outcome(0).audit_outcome_slug(), "ok");
        let cases = [
            (
                ProbeFailure::Auth {
                    http_status: 401,
                    ga_code: "none".into(),
                    body_sample: String::new(),
                },
                "auth",
            ),
            (
                ProbeFailure::RateLimited {
                    http_status: 429,
                    retry_after_secs: None,
                },
                "rate_limited",
            ),
            (
                ProbeFailure::VendorReject {
                    http_status: 200,
                    ga_code: "GA001".into(),
                    body_sample: String::new(),
                },
                "vendor_reject",
            ),
            (
                ProbeFailure::HttpError {
                    http_status: 503,
                    body_sample: String::new(),
                },
                "http_error",
            ),
            (
                ProbeFailure::Parse {
                    http_status: 200,
                    detail: String::new(),
                },
                "parse",
            ),
            (ProbeFailure::Oversize { declared_len: None }, "oversize"),
            (
                ProbeFailure::Transport {
                    detail: String::new(),
                },
                "transport",
            ),
            (ProbeFailure::TokenUnavailable, "token_read"),
        ];
        for (failure, slug) in cases {
            assert_eq!(failure_outcome(failure).audit_outcome_slug(), slug);
        }
    }

    #[test]
    fn test_report_debug_never_contains_jwt() {
        let jwt = format!(
            "eyJ{}.eyJ{}.sig{}",
            "h".repeat(40),
            "p".repeat(40),
            "s".repeat(40)
        );
        let body =
            format!(r#"{{"status":"FAILURE","error":{{"code":"GA000","message":"{jwt}"}}}}"#);
        let failure = classify_response(500, None, &body).expect_err("must fail");
        let outcome = failure_outcome(failure);
        let debug = format!("{outcome:?}");
        assert!(!debug.contains("eyJ"), "Debug must never leak a JWT shape");
        assert!(!outcome.operator_summary().contains("eyJ"));
    }

    // -- source-scan self-ratchets (production region = above the first
    //    column-0 `#[cfg(test)]`, the house split) ---------------------------

    fn production_region() -> &'static str {
        let source = include_str!("user.rs");
        source
            .split("\n#[cfg(test)]")
            .next()
            .expect("split always yields a first segment")
    }

    #[test]
    fn test_ratchet_get_only_no_mutating_verbs() {
        let prod = production_region();
        for needle in [".post(", ".put(", ".delete(", ".request("] {
            assert!(
                !prod.contains(needle),
                "readiness is a GET-only surface — `{needle}` is banned"
            );
        }
    }

    #[test]
    fn test_ratchet_single_v1_literal_and_no_user_detail() {
        let prod = production_region();
        assert_eq!(
            prod.matches("\"/v1/").count(),
            1,
            "exactly one /v1/ literal — the margins probe endpoint"
        );
        // Assembled at runtime so this file never carries the banned path.
        let banned = ["/v1/us", "er/detail"].concat();
        assert!(
            !prod.contains(&banned),
            "the undocumented user-surface GET path must appear NOWHERE"
        );
    }

    #[test]
    fn test_ratchet_expose_secret_exactly_once() {
        let prod = production_region();
        assert_eq!(
            prod.matches("expose_secret()").count(),
            1,
            "expose_secret() must appear exactly once (probe_request)"
        );
    }

    #[test]
    fn test_ratchet_no_unwrap_expect_println_config_ssm_storage() {
        let prod = production_region();
        for needle in [
            ".unwrap()",
            ".expect(",
            "println!",
            "AppConfig",
            "figment",
            "aws_sdk",
            "tickvault_storage",
        ] {
            assert!(
                !prod.contains(needle),
                "`{needle}` is banned in the production region"
            );
        }
    }

    #[test]
    fn test_ratchet_every_error_macro_carries_code_field() {
        let prod = production_region();
        let mut sites = 0;
        for (idx, _) in prod.match_indices("error!(") {
            sites += 1;
            let window = &prod[idx..(idx + 400).min(prod.len())];
            assert!(
                window.contains("code = ErrorCode::"),
                "every error! must carry code = ErrorCode::X.code_str()"
            );
        }
        assert!(sites >= 1, "the degraded emit must exist");
    }
}
