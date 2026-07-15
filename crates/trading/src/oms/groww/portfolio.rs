//! Groww PORTFOLIO area (`GROWW-PORT-*`) — the one-shot boot FIELD-INVENTORY
//! probe over the documented portfolio GETs (plan item 6c.2).
//!
//! # GROWW — Dhan portfolio semantics do NOT transfer
//! No camelCase serde assumption, no `exit-all`, no `convert-position`, no
//! `positionType` vocabulary. The Groww 17/12-field row schemas are
//! UNVERIFIED-LIVE (`docs/groww-ref/16-orders-margins-portfolio.md` — capture
//! page 04 never vendored), which is exactly why this module ships a probe
//! FIRST: fetch once, record field NAMES + JSON types only (never values),
//! classify PASS/PASS-EMPTY/PARTIAL/FAIL, and keep every poller OFF until the
//! live inventory passes with a dated note.
//!
//! # Gates (§39.2 live-fire lattice)
//! - Gate 2: this file compiles ONLY under the non-default `groww_orders`
//!   feature (the `oms::groww` subtree).
//! - Gate 1: every entry point takes the caller's flag view
//!   ([`PortfolioProbeParams`]) and refuses INERT when
//!   `groww_orders.portfolio_read` is false (positions/holdings) — the margin
//!   leg additionally requires `groww_orders.margin_read`. The probe also
//!   respects `probe_and_report`.
//! - Gates 3/4 are untouched: everything here is READ-ONLY GETs; no mutating
//!   request exists in this file.
//!
//! # Market-hours semantics (Gate-1 note — deliberate, no behavioral gate)
//! Per the build lead's box-window ruling, the ONE-SHOT boot probe is
//! sanctioned anywhere inside the instance box window (the 08:30–16:30 IST
//! auto start/stop window) — this module deliberately adds NO market-hours /
//! trading-day gate in this PR. The 6c.3 T-tier scheduler carries the
//! in-session gates ([09:15, 15:30) IST + trading-day awareness) for the
//! recurring pollers. A probe firing pre-open honestly reads whatever book
//! the broker serves — plausibly empty; see [`ProbeVerdict::PassEmpty`] and
//! the `ok_empty_cold_boot` label.
//!
//! # Declared deltas vs the design doc (dated 2026-07-15)
//! - NO raw capture file: the design's "one secret-redacted size-capped raw
//!   capture `data/portfolio/probe-<date>.json`" is deliberately NOT written
//!   — stricter than design: field NAMES + types are the ONLY payload
//!   derivative that survives the fetch fn.
//!
//! Gate-5 note: the endpoint path strings below are ALLOWED here only because
//! this file lives under `oms/groww/`
//! (`crates/common/tests/groww_order_lattice_guard.rs` path exemption). Never
//! copy these strings — code or doc comment — into any file outside this
//! subtree.
//!
//! Runbook (lands with the scaffold PR):
//! `.claude/rules/project/groww-portfolio-error-codes.md`. Canonical snapshot
//! types: `tickvault_common::portfolio_types` (6c.1). No QuestDB write and no
//! snapshot publish happens in this module yet — the poller/persistence PR
//! (6c.3) owns those.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::sanitize_ilp_string;
use tracing::{error, info};

// ---------------------------------------------------------------------------
// Endpoint constants — the documented portfolio GETs (design §1 rows 1-4).
// INSIDE this file by design (Gate-5 path-based exemption; §39.3 ⚠3).
// ---------------------------------------------------------------------------

/// Positions (all, per segment): `GET ?segment=CASH|FNO|COMMODITY`.
pub const GROWW_PORTFOLIO_POSITIONS_URL: &str = "https://api.groww.in/v1/positions/user"; // APPROVED: oms/groww/portfolio.rs is the single portfolio-endpoint source (Gate-5 path exemption, §39.3)
/// Position for one symbol — NEVER polled; reserved for the future flatten
/// per-leg pre-submit re-check (design F6, PR-L). Const pinned now so the
/// endpoint inventory lives in ONE place.
pub const GROWW_PORTFOLIO_POSITION_FOR_SYMBOL_URL: &str =
    "https://api.groww.in/v1/positions/trading-symbol"; // APPROVED: oms/groww/portfolio.rs is the single portfolio-endpoint source (Gate-5 path exemption, §39.3)
/// Holdings (probe + T4 daily only).
pub const GROWW_PORTFOLIO_HOLDINGS_URL: &str = "https://api.groww.in/v1/holdings/user"; // APPROVED: oms/groww/portfolio.rs is the single portfolio-endpoint source (Gate-5 path exemption, §39.3)
/// User margin/funds — ACCOUNT-POOLED with the co-tenant program (design N5).
pub const GROWW_PORTFOLIO_MARGIN_URL: &str = "https://api.groww.in/v1/margins/detail/user"; // APPROVED: oms/groww/portfolio.rs is the single portfolio-endpoint source (Gate-5 path exemption, §39.3)

/// `X-API-VERSION` header value on every Groww REST call (doc-cited).
const GROWW_PORTFOLIO_API_VERSION: &str = "1.0";
/// Per-request timeout (design §3.4: 5s per request).
const PROBE_REQUEST_TIMEOUT_SECS: u64 = 5;
/// Sequential inter-request gap (design §3.4: ≥250ms — never bursts the
/// pooled Non-Trading bucket shared with the co-tenant).
const PROBE_INTER_REQUEST_GAP_MS: u64 = 250;
/// Response body size cap (design §3.4). The body is read INCREMENTALLY
/// (`Response::chunk`) and the read is ABORTED the moment the accumulated
/// bytes exceed this cap (`body_too_large` leg failure) — peak RAM per leg
/// is bounded by this constant, never by the peer.
const PROBE_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Single bounded backoff before the ONE retry of a rate-limited (HTTP 429)
/// leg. The runbook's `rate_limited` contract is "counted + one bounded
/// backoff, never out-polled"; for the ONE-SHOT boot probe a short 5s wait
/// is deliberate (a 30s wait would stall boot for marginal benefit — the
/// next boot re-probes anyway).
const PROBE_RATE_LIMIT_RETRY_SECS: u64 = 5;
/// Field-inventory recursion depth cap (defensive — a pathological nested
/// payload can never stack-overflow the probe). Exceeding it sets the
/// honest `inventory_truncated` flag.
const INVENTORY_MAX_DEPTH: usize = 8;
/// Field-inventory entry cap per leg (defensive — bounded RAM). Exceeding
/// it sets the honest `inventory_truncated` flag, never a silent drop.
const INVENTORY_MAX_ENTRIES: usize = 512;
/// Per-entry dotted-path length cap (chars) — a hostile 2 MiB object key
/// can never become a 2 MiB report string (truncated with a `…` marker).
const INVENTORY_PATH_MAX_CHARS: usize = 120;
/// Per-key-component cap (chars) fed into path building.
const INVENTORY_KEY_MAX_CHARS: usize = 48;
/// GA error-code cap (chars): a real Groww GA code is short alphanumerics;
/// anything longer or outside `[A-Za-z0-9_-]` is attacker-shaped and is
/// replaced wholesale (never stored/logged raw).
const GA_CODE_MAX_CHARS: usize = 32;
/// Defensive char cap on the coalesced failed-legs summary log field.
const FAILED_SUMMARY_MAX_CHARS: usize = 400;

// ---------------------------------------------------------------------------
// Gate-1 parameter view
// ---------------------------------------------------------------------------

/// The caller-supplied Gate-1 flag view for the probe.
///
/// TODO(§39.3): converge onto the build lead's nested
/// `GrowwOrdersConfig.portfolio` sub-struct (`GrowwPortfolioConfig`,
/// TOML `[groww_orders.portfolio]`) once the scaffold PR lands — this local
/// struct exists only so 6c.2 needs no edit to the build-lead-owned
/// `config.rs`. The flag SEMANTICS are already the branch contract:
/// `portfolio_read` / `margin_read` are the existing `GrowwOrdersConfig`
/// keys; `probe_and_report` is the §B spec default-true probe switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortfolioProbeParams {
    /// `groww_orders.portfolio_read` — gates the positions + holdings legs
    /// AND the probe as a whole (Gate 1).
    pub portfolio_read: bool,
    /// `groww_orders.margin_read` — gates the margin leg only.
    pub margin_read: bool,
    /// The probe switch (§B spec `probe_and_report`, serde default true).
    pub probe_and_report: bool,
}

/// Pure Gate-1 refusal: `Some(reason)` when the probe must refuse INERT
/// (no request, no token read, no client build), `None` when it may run.
fn gate_refusal(params: &PortfolioProbeParams) -> Option<&'static str> {
    if !params.portfolio_read {
        return Some("portfolio_read=false");
    }
    if !params.probe_and_report {
        return Some("probe_and_report=false");
    }
    None
}

// ---------------------------------------------------------------------------
// Typed panic-free HTTP client (house pattern — see the HTTP-CLIENT-01
// doctrine in crates/storage/src/http_client.rs: builder Err maps to a typed
// error; `Client::new()` — which PANICS on builder failure — is never used).
// ---------------------------------------------------------------------------

/// Typed, panic-free error for a failed `reqwest::ClientBuilder::build()`.
/// The message is userinfo-redacted (builder errors can embed `HTTPS_PROXY`
/// URLs carrying Basic-Auth credentials).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortfolioClientBuildError {
    message: String,
}

impl fmt::Display for PortfolioClientBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "portfolio reqwest client build failed: {}", self.message)
    }
}

impl std::error::Error for PortfolioClientBuildError {}

/// Redact URL userinfo (`scheme://user:pass@host` → `scheme://***@host`)
/// before a builder error message is stored/logged (mirror of the
/// storage `http_client::redact_userinfo` discipline).
fn redact_userinfo(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    while let Some(idx) = rest.find("://") {
        let (head, tail) = rest.split_at(idx + "://".len());
        out.push_str(head);
        // Scan to the first whitespace/`)` (NOT the first `/`): an
        // unencoded `/` inside a password would otherwise split the
        // authority BEFORE the `@` and leak the secret tail (hostile
        // review #2 finding 6 — over-redaction of a path `@` is the
        // acceptable trade).
        let boundary = tail
            .find(|c: char| c.is_whitespace() || c == ')')
            .unwrap_or(tail.len());
        let authority = &tail[..boundary];
        if let Some(at) = authority.rfind('@') {
            out.push_str("***");
            rest = &tail[at..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// Build the probe client with NO panic path (typed error on builder
/// failure). `Policy::none()` is the house §18 doctrine for every Groww
/// bearer-carrying request (socket_token.rs precedent): a redirect would
/// re-send the bearer — refuse to follow ANY; the 3xx classifies as a hard
/// `redirect` leg failure instead.
fn build_portfolio_client() -> Result<Client, PortfolioClientBuildError> {
    Client::builder()
        .timeout(Duration::from_secs(PROBE_REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| PortfolioClientBuildError {
            message: redact_userinfo(&e.to_string()),
        })
}

// ---------------------------------------------------------------------------
// Probe report types
// ---------------------------------------------------------------------------

/// One inventoried field: dotted path + JSON type name. NEVER a value —
/// redaction discipline: the probe records names/types only.
///
/// CAVEAT (redaction envelope, hostile review #2 finding 3): when a broker
/// keys objects BY DATA (a map-keyed row container like
/// `{"positions": {"ACC1234": {…}}}`), the KEY is data and appears in
/// `path`. The "names only" guarantee therefore holds for the DOCUMENTED
/// array-row schemas; a map-keyed schema would surface BOUNDED, SANITIZED
/// key text here (control/BiDi/zero-width stripped, component capped at
/// [`INVENTORY_KEY_MAX_CHARS`] chars, whole path capped at
/// [`INVENTORY_PATH_MAX_CHARS`]). 6c.3 MUST NOT present the inventory as
/// value-free without this caveat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldEntry {
    /// Dotted path inside the envelope `payload` (arrays inventoried via
    /// their FIRST element under a `[]` marker — rows share one schema).
    /// Sanitized + length-capped at build time (see the struct caveat).
    pub path: String,
    /// JSON type name: `object`/`array`/`string`/`number`/`boolean`/`null`.
    pub json_type: &'static str,
}

/// One probe leg's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeLegOutcome {
    /// The leg answered and its payload inventoried (names + types only),
    /// with rows observed (row-bearing legs) or no row container expected
    /// (the margin leg, `row_count == None`).
    Inventory {
        /// Field-name inventory of the envelope payload.
        fields: Vec<FieldEntry>,
        /// Row count of the DOCUMENTED row container (`Some(n)`, n > 0 by
        /// construction — zero rows classifies [`Self::OkEmpty`]); `None`
        /// for the margin leg (no row container expected).
        row_count: Option<usize>,
        /// True when the inventory hit the entry/depth cap — fields exist
        /// beyond what is recorded (honest overflow, never silent).
        inventory_truncated: bool,
    },
    /// The leg answered SUCCESS with a recognized-but-EMPTY row container —
    /// `ok_empty` (audit Rule 11: never conflated with failure, and never
    /// conflated with a rows-observed inventory either).
    OkEmpty {
        /// Field-name inventory of the (empty-book) payload.
        fields: Vec<FieldEntry>,
        /// True when the inventory hit the entry/depth cap.
        inventory_truncated: bool,
    },
    /// Gate-1 refusal for this leg (e.g. `margin_read=false`) — no request.
    SkippedGate {
        /// The refusing flag, fixed string (carried in the leg plan).
        reason: &'static str,
    },
    /// The leg failed. `detail` carries ONLY a fixed classification /
    /// HTTP status digits / a sanitized bounded GA error code
    /// ([`GA_CODE_MAX_CHARS`], charset `[A-Za-z0-9_-]`) — never body values.
    Failed {
        /// Fixed failure stage label.
        stage: &'static str,
        /// Bounded non-value detail (status code digits or sanitized GA code).
        detail: String,
    },
}

/// One probe leg: fixed leg label + outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeLeg {
    /// Fixed leg label (`positions_cash` / `positions_fno` /
    /// `positions_commodity` / `holdings` / `margin`).
    pub leg: &'static str,
    /// What happened.
    pub outcome: ProbeLegOutcome,
}

/// Per-row presence of a probed field name across position rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldPresence {
    /// Named on every row.
    AllRows,
    /// Named on some rows only.
    SomeRows,
    /// Named on no row.
    Absent,
    /// No rows to inspect (empty book — presence UNKNOWN, stated).
    NoRows,
}

/// The design-§3.1 probe checklist items (i) + (ii): does the wire name
/// `segment` and `product` per position row? (A per-row `segment` field is
/// the honest static signal for checklist (i) "is the `?segment=` query
/// param required" — the definitive param-omission answer needs the live
/// probe's optional extra request, a 6c.3 follow-up.)
///
/// PROVENANCE HONESTY (hostile review #3 finding 9): presence is computed
/// over the combined rows of the positions legs that actually INVENTORIED —
/// `contributing_legs` says how many of the 3 (CASH/FNO/COMMODITY) fed the
/// checks. A reader must not read `AllRows` as covering a leg that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeRowChecks {
    /// Is a `segment` field named per position row (contributing legs)?
    pub segment_field_per_row: FieldPresence,
    /// Is a `product` field named per position row (contributing legs)?
    pub product_field_per_row: FieldPresence,
    /// How many positions legs contributed payloads to these checks (0-3).
    pub contributing_legs: u8,
}

/// Probe verdict. PASS = field-inventory parses WITH rows observed (design
/// N9 — NEVER gated on any `net_qty` cross-check; derivation checks are
/// opportunistic and live elsewhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// Every attempted leg succeeded AND ≥1 row-bearing leg observed rows —
    /// the row schema was actually learned. The ONLY verdict that can
    /// certify the 6c.5 pipeline-enable flip.
    Pass,
    /// Every attempted leg succeeded but ZERO rows were observed anywhere
    /// (plausible in dry-run: the whole broker book empty at probe time).
    /// The row-schema inventory is EMPTY — this deliberately does NOT
    /// certify the 6c.5 enable flip (audit Rule 11: never a schema-blind
    /// PASS). Re-probe on a day with book activity.
    PassEmpty,
    /// At least one attempted leg succeeded; at least one failed.
    Partial,
    /// No attempted leg succeeded (schema/transport dead) — keeps the
    /// pipeline OFF. Zero attempted legs also lands here (fail-closed
    /// floor).
    Fail,
}

/// The typed probe report (field-inventory only — no values anywhere).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    /// Every leg in fire order.
    pub legs: Vec<ProbeLeg>,
    /// PASS/PASS-EMPTY/PARTIAL/FAIL classification over the attempted legs.
    pub verdict: ProbeVerdict,
    /// Checklist (i)+(ii) row checks, when position rows were inspectable.
    pub row_checks: Option<ProbeRowChecks>,
    /// The runbook's `ok_empty_cold_boot` label: true ONLY when the verdict
    /// is [`ProbeVerdict::PassEmpty`] AND the margin leg inventoried AND
    /// every documented margin USAGE figure read zero/absent — provisionally
    /// flat, NEVER "confirmed flat" (audit Rule 11). False whenever margin
    /// was skipped/failed or showed usage (a nonzero usage with an empty
    /// queried book is the false-flat contradiction, not a cold boot).
    pub ok_empty_cold_boot: bool,
}

/// The probe entry-point result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Gate-1 refusal — INERT by design: zero requests, zero token reads.
    RefusedInert {
        /// The refusing flag.
        reason: &'static str,
    },
    /// The probe ran; here is the field-inventory report.
    Report(ProbeReport),
}

/// Probe entry-point error (pre-leg failures — a single leg failing is a
/// [`ProbeLegOutcome::Failed`], not this).
#[derive(Debug)]
pub enum ProbeError {
    /// SSM token read failed — a BOUNDED-RETRY TRANSIENT (design row 7:
    /// retry ≥60s ladder / next-boot re-probe), NEVER day-killing. Only a
    /// schema-inventory FAIL verdict keeps the pipeline OFF.
    TokenUnavailable {
        /// Fixed classification of the read failure (no secret echo).
        detail: String,
    },
    /// The reqwest client could not be built (typed, panic-free).
    ClientBuild(PortfolioClientBuildError),
}

impl ProbeError {
    /// True when the error is a bounded-retry transient the caller re-probes
    /// past (≥60s ladder / next boot) rather than a day-killing verdict.
    #[must_use]
    pub fn is_bounded_transient(&self) -> bool {
        match self {
            // Token-read failure: the minter Lambda owns the daily mint; we
            // re-read, never mint, never give up the day (design row 7).
            Self::TokenUnavailable { .. } => true,
            // Client build failure: host fd/TLS/resolver pressure — the
            // next attempt retries the build (HTTP-CLIENT-01 doctrine).
            Self::ClientBuild(_) => true,
        }
    }
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TokenUnavailable { detail } => {
                write!(f, "groww portfolio probe: token unavailable: {detail}")
            }
            Self::ClientBuild(e) => write!(f, "groww portfolio probe: {e}"),
        }
    }
}

impl std::error::Error for ProbeError {}

// ---------------------------------------------------------------------------
// Pure probe pipeline (unit-tested on canned fixtures — no network)
// ---------------------------------------------------------------------------

/// JSON type name for one value (fixed strings — never the value).
const fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Sanitize one object-key path component: strip control/BiDi/zero-width/BOM
/// (reuses the common `sanitize_ilp_string` char class — the
/// `sanitize_audit_string` / FEED-REJECT-01 precedent) and cap the component
/// length. Keys ARE attacker-influenceable text (see the [`FieldEntry`]
/// map-keys-as-data caveat); they must be bounded + clean before they can
/// enter a report headed for logs/Telegram/QuestDB in 6c.3.
fn sanitize_path_key(key: &str) -> String {
    sanitize_ilp_string(key)
        .chars()
        .take(INVENTORY_KEY_MAX_CHARS)
        .collect()
}

/// Bound a full dotted path to [`INVENTORY_PATH_MAX_CHARS`] chars, with an
/// explicit `…` truncation marker (honest, never a silent cut).
fn bounded_entry_path(path: &str) -> String {
    let mut out: String = path.chars().take(INVENTORY_PATH_MAX_CHARS).collect();
    if path.chars().nth(INVENTORY_PATH_MAX_CHARS).is_some() {
        out.push('…');
    }
    out
}

/// Recursive field-NAME inventory of a payload. Records dotted paths + JSON
/// type names ONLY (redaction discipline — values never leave this fn).
/// Arrays are inventoried via their FIRST element under a `[]` marker (rows
/// share one schema). Depth/entry capped defensively; the returned bool is
/// the honest `inventory_truncated` flag (true when ANY cap cut fields).
fn field_inventory(payload: &Value) -> (Vec<FieldEntry>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    inventory_walk(payload, "payload", 0, &mut out, &mut truncated);
    (out, truncated)
}

fn inventory_walk(
    v: &Value,
    path: &str,
    depth: usize,
    out: &mut Vec<FieldEntry>,
    truncated: &mut bool,
) {
    if out.len() >= INVENTORY_MAX_ENTRIES {
        *truncated = true;
        return;
    }
    out.push(FieldEntry {
        path: bounded_entry_path(path),
        json_type: json_type_name(v),
    });
    if depth >= INVENTORY_MAX_DEPTH {
        // Anything below the depth cap goes unrecorded — flag it honestly
        // when children actually exist.
        let has_children = match v {
            Value::Object(map) => !map.is_empty(),
            Value::Array(items) => !items.is_empty(),
            _ => false,
        };
        if has_children {
            *truncated = true;
        }
        return;
    }
    match v {
        Value::Object(map) => {
            for (key, child) in map {
                let child_path = format!("{path}.{}", sanitize_path_key(key));
                inventory_walk(child, &child_path, depth + 1, out, truncated);
            }
        }
        Value::Array(items) => {
            if let Some(first) = items.first() {
                let child_path = format!("{path}[]");
                inventory_walk(first, &child_path, depth + 1, out, truncated);
            }
        }
        _ => {}
    }
}

/// Bound + whitelist a broker GA error code before it may be stored/logged
/// (hostile review #2 finding 2 — the code is BODY-derived text). A real GA
/// code is short `[A-Za-z0-9_-]`; anything else is replaced WHOLESALE with a
/// fixed marker (never a partial echo of hostile bytes).
fn sanitize_ga_code(code: &str) -> String {
    let well_formed = !code.is_empty()
        && code.chars().count() <= GA_CODE_MAX_CHARS
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if well_formed {
        code.to_owned()
    } else {
        "GA code malformed".to_owned()
    }
}

/// Envelope classification (design §1 common wire contract):
/// `{"status":"SUCCESS","payload":{…}}` vs
/// `{"status":"FAILURE","error":{code,…}}`. SUCCESS+empty payload is
/// `ok_empty` territory — it still inventories (never conflated with
/// failure, audit Rule 11). SUCCESS with `payload: null` or ABSENT is
/// Malformed — a null payload must never classify Success (fix G).
enum EnvelopeOutcome<'a> {
    /// SUCCESS — the payload to inventory (never `Value::Null`).
    Success(&'a Value),
    /// FAILURE — the sanitized GA error code when named (a code, never a
    /// value; bounded + charset-whitelisted at THIS choke point).
    Failure { ga_code: Option<String> },
    /// Neither shape — schema drift on the envelope itself.
    Malformed,
}

fn classify_envelope(body: &Value) -> EnvelopeOutcome<'_> {
    let Some(status) = body.get("status").and_then(Value::as_str) else {
        return EnvelopeOutcome::Malformed;
    };
    match status {
        "SUCCESS" => match body.get("payload") {
            None | Some(Value::Null) => EnvelopeOutcome::Malformed,
            Some(payload) => EnvelopeOutcome::Success(payload),
        },
        "FAILURE" => EnvelopeOutcome::Failure {
            ga_code: body
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(Value::as_str)
                .map(sanitize_ga_code),
        },
        _ => EnvelopeOutcome::Malformed,
    }
}

// ---------------------------------------------------------------------------
// Leg plan (pure — fix D: Gate-1 leg view testable without a network) + the
// explicit per-endpoint row binding (fix E: never an alphabetically-first
// array).
// ---------------------------------------------------------------------------

/// What kind of payload a leg serves — drives the DOCUMENTED row binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegKind {
    Positions,
    Holdings,
    Margin,
}

/// One row of the probe leg plan — pure data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProbeLegPlan {
    leg: &'static str,
    url: &'static str,
    query: Option<(&'static str, &'static str)>,
    /// Gate-1 verdict for this leg; `false` ⇒ SkippedGate, NEVER fetched.
    allowed: bool,
    /// The flag that gates this leg (carried in the plan so a future gated
    /// leg can never inherit a wrong hardcoded skip reason).
    gate_reason: &'static str,
    kind: LegKind,
}

/// Number of probe legs: positions CASH + FNO + COMMODITY (the design's
/// N1 shared-account blind-spot closure — the probe covers all 3 segments;
/// T-tier COMMODITY scheduling stays 6c.3) + holdings + margin.
const PROBE_PLAN_LEGS: usize = 5;

/// Build the probe leg plan from the Gate-1 flag view. PURE — the margin
/// leg's `allowed` is exactly `params.margin_read`; the other legs are
/// gated by `portfolio_read` at the probe entry (which refused before this
/// fn can run when false).
fn build_probe_plan(params: &PortfolioProbeParams) -> [ProbeLegPlan; PROBE_PLAN_LEGS] {
    [
        ProbeLegPlan {
            leg: "positions_cash",
            url: GROWW_PORTFOLIO_POSITIONS_URL,
            query: Some(("segment", "CASH")),
            allowed: true,
            gate_reason: "portfolio_read=false",
            kind: LegKind::Positions,
        },
        ProbeLegPlan {
            leg: "positions_fno",
            url: GROWW_PORTFOLIO_POSITIONS_URL,
            query: Some(("segment", "FNO")),
            allowed: true,
            gate_reason: "portfolio_read=false",
            kind: LegKind::Positions,
        },
        ProbeLegPlan {
            leg: "positions_commodity",
            url: GROWW_PORTFOLIO_POSITIONS_URL,
            query: Some(("segment", "COMMODITY")),
            allowed: true,
            gate_reason: "portfolio_read=false",
            kind: LegKind::Positions,
        },
        ProbeLegPlan {
            leg: "holdings",
            url: GROWW_PORTFOLIO_HOLDINGS_URL,
            query: None,
            allowed: true,
            gate_reason: "portfolio_read=false",
            kind: LegKind::Holdings,
        },
        ProbeLegPlan {
            leg: "margin",
            url: GROWW_PORTFOLIO_MARGIN_URL,
            query: None,
            allowed: params.margin_read,
            gate_reason: "margin_read=false",
            kind: LegKind::Margin,
        },
    ]
}

/// Documented row-container keys per row-bearing endpoint
/// (`docs/groww-ref/16` §1 rows 9-11: positions rows / holdings rows —
/// the SDK envelope nests them under these payload keys). Tried IN ORDER;
/// a bare top-level array payload is also accepted. NOTHING ELSE binds —
/// the probe never guesses an arbitrary (alphabetically-first) array
/// (hostile reviews #1 finding 4 / #3 finding 5).
const POSITIONS_ROW_CONTAINER_KEYS: [&str; 1] = ["positions"];
const HOLDINGS_ROW_CONTAINER_KEYS: [&str; 1] = ["holdings"];

/// Explicit row binding for one leg payload.
enum RowBinding<'a> {
    /// The documented container (or top-level array) bound.
    Rows(&'a [Value]),
    /// The leg has no row container by design (margin).
    NotRowBearing,
    /// A row-bearing leg whose payload matched NO documented container —
    /// classified `envelope_unrecognized`, never silently bound to some
    /// other array (the NoRows-while-rows-exist hole).
    Unrecognized,
}

fn bind_rows(kind: LegKind, payload: &Value) -> RowBinding<'_> {
    let keys: &[&str] = match kind {
        LegKind::Positions => &POSITIONS_ROW_CONTAINER_KEYS,
        LegKind::Holdings => &HOLDINGS_ROW_CONTAINER_KEYS,
        LegKind::Margin => return RowBinding::NotRowBearing,
    };
    if let Value::Array(items) = payload {
        return RowBinding::Rows(items);
    }
    if let Value::Object(map) = payload {
        for key in keys {
            if let Some(Value::Array(items)) = map.get(*key) {
                return RowBinding::Rows(items);
            }
        }
    }
    RowBinding::Unrecognized
}

/// Classify a SUCCESS payload into the leg outcome: inventory + explicit
/// row binding. Zero rows on a row-bearing leg is `OkEmpty` (fix G);
/// an unrecognized envelope is a `Failed` leg, never a guess (fix E).
fn leg_outcome_from_payload(kind: LegKind, payload: &Value) -> ProbeLegOutcome {
    let (fields, inventory_truncated) = field_inventory(payload);
    match bind_rows(kind, payload) {
        RowBinding::NotRowBearing => ProbeLegOutcome::Inventory {
            fields,
            row_count: None,
            inventory_truncated,
        },
        RowBinding::Rows([]) => ProbeLegOutcome::OkEmpty {
            fields,
            inventory_truncated,
        },
        RowBinding::Rows(rows) => ProbeLegOutcome::Inventory {
            fields,
            row_count: Some(rows.len()),
            inventory_truncated,
        },
        RowBinding::Unrecognized => ProbeLegOutcome::Failed {
            stage: "envelope_unrecognized",
            detail: "no documented row container".to_owned(),
        },
    }
}

/// Per-row presence of `field` across `rows` (object rows only).
fn check_rows_field_presence(rows: &[Value], field: &str) -> FieldPresence {
    if rows.is_empty() {
        return FieldPresence::NoRows;
    }
    let named = rows.iter().filter(|r| r.get(field).is_some()).count();
    if named == rows.len() {
        FieldPresence::AllRows
    } else if named > 0 {
        FieldPresence::SomeRows
    } else {
        FieldPresence::Absent
    }
}

/// Checklist (i)+(ii) row checks over the positions payloads that
/// inventoried (CASH/FNO/COMMODITY — `contributing_legs` counts them).
fn probe_row_checks(position_payloads: &[Value]) -> Option<ProbeRowChecks> {
    if position_payloads.is_empty() {
        return None;
    }
    let mut rows: Vec<Value> = Vec::new();
    for payload in position_payloads {
        if let RowBinding::Rows(found) = bind_rows(LegKind::Positions, payload) {
            rows.extend(found.iter().cloned()); // APPROVED: cold path — one-shot boot probe, not the tick hot path
        }
    }
    Some(ProbeRowChecks {
        segment_field_per_row: check_rows_field_presence(&rows, "segment"),
        product_field_per_row: check_rows_field_presence(&rows, "product"),
        contributing_legs: u8::try_from(position_payloads.len()).unwrap_or(u8::MAX),
    })
}

/// Documented §3.1 margin USAGE field names (`docs/groww-ref/16` — the
/// response nests `fno_margin_details` / `equity_margin_details` /
/// `commodity_margin_details`, so the scan is recursive + depth-bounded).
/// Values are read ONLY to derive the cold-boot-quiet bool — never stored,
/// never logged (the f64 read is a transient zero-comparison, not money
/// storage; `data-integrity.md` paise rule governs STORED money).
const MARGIN_USAGE_FIELD_NAMES: [&str; 9] = [
    "net_margin_used",
    "net_fno_margin_used",
    "span_margin_used",
    "exposure_margin_used",
    "net_equity_margin_used",
    "cnc_margin_used",
    "mis_margin_used",
    "commodity_span_margin",
    "commodity_exposure_margin",
];

/// True when NO documented margin usage figure is positive — the
/// `ok_empty_cold_boot` labeling input (all-legs-empty + margin-quiet).
fn margin_shows_no_usage(payload: &Value) -> bool {
    !margin_usage_nonzero(payload, 0)
}

fn margin_usage_nonzero(v: &Value, depth: usize) -> bool {
    let Value::Object(map) = v else {
        return false;
    };
    for (key, child) in map {
        if MARGIN_USAGE_FIELD_NAMES.contains(&key.as_str())
            && child.as_f64().is_some_and(|n| n > 0.0)
        {
            return true;
        }
        if depth < INVENTORY_MAX_DEPTH
            && matches!(child, Value::Object(_))
            && margin_usage_nonzero(child, depth + 1)
        {
            return true;
        }
    }
    false
}

/// PASS/PASS-EMPTY/PARTIAL/FAIL over the legs. Skipped-gate legs are NOT
/// attempted. PASS requires rows OBSERVED on ≥1 row-bearing leg
/// (field-inventory ONLY — design N9: no `net_qty` / value-level
/// cross-check ever gates this verdict); an all-succeeded-but-zero-rows run
/// is PASS-EMPTY (fix G — never a schema-blind Pass). Zero attempted legs
/// classifies FAIL (fail-closed floor).
fn classify_probe_verdict(legs: &[ProbeLeg]) -> ProbeVerdict {
    let mut succeeded = 0_usize;
    let mut failed = 0_usize;
    let mut rows_observed = false;
    for leg in legs {
        match &leg.outcome {
            ProbeLegOutcome::Inventory { row_count, .. } => {
                succeeded += 1;
                if row_count.is_some() {
                    rows_observed = true;
                }
            }
            ProbeLegOutcome::OkEmpty { .. } => succeeded += 1,
            ProbeLegOutcome::Failed { .. } => failed += 1,
            ProbeLegOutcome::SkippedGate { .. } => {}
        }
    }
    if succeeded == 0 {
        ProbeVerdict::Fail
    } else if failed > 0 {
        ProbeVerdict::Partial
    } else if rows_observed {
        ProbeVerdict::Pass
    } else {
        ProbeVerdict::PassEmpty
    }
}

/// Static counter label for a leg outcome (static labels only — never a
/// dynamic string into the metrics registry).
const fn leg_outcome_label(outcome: &ProbeLegOutcome) -> &'static str {
    match outcome {
        ProbeLegOutcome::Inventory { .. } => "inventory",
        ProbeLegOutcome::OkEmpty { .. } => "ok_empty",
        ProbeLegOutcome::SkippedGate { .. } => "skipped_gate",
        ProbeLegOutcome::Failed { .. } => "failed",
    }
}

/// Coalesce the failed legs into ONE bounded summary string (runbook §1:
/// `cycle_failed` is emitted ONCE per probe run, never per leg). Every
/// `detail` is already fixed/bounded (status digits, fixed strings, or the
/// [`GA_CODE_MAX_CHARS`]-sanitized GA code); the whole summary is char-capped
/// defensively anyway.
fn summarize_failed_legs(legs: &[ProbeLeg]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for leg in legs {
        if let ProbeLegOutcome::Failed { stage, detail } = &leg.outcome {
            parts.push(format!("{}:{stage}:{detail}", leg.leg));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(
            parts
                .join(", ")
                .chars()
                .take(FAILED_SUMMARY_MAX_CHARS)
                .collect(),
        )
    }
}

// ---------------------------------------------------------------------------
// Transport (cold path; sequential; bounded)
// ---------------------------------------------------------------------------

/// Internal per-leg fetch result: the report outcome + the parsed payload
/// (held in RAM briefly for the row checks / cold-boot labeling, then
/// dropped — never logged, never persisted in this PR).
struct LegFetch {
    outcome: ProbeLegOutcome,
    payload: Option<Value>,
}

impl LegFetch {
    fn failed(stage: &'static str, detail: &str) -> Self {
        Self {
            outcome: ProbeLegOutcome::Failed {
                stage,
                detail: detail.to_owned(),
            },
            payload: None,
        }
    }
}

async fn fetch_probe_leg(client: &Client, row: ProbeLegPlan, token: &SecretString) -> LegFetch {
    let mut rate_limit_retried = false;
    let resp = loop {
        let mut req = client
            .get(row.url)
            .header("Authorization", format!("Bearer {}", token.expose_secret()))
            .header("X-API-VERSION", GROWW_PORTFOLIO_API_VERSION)
            .header("Accept", "application/json");
        if let Some((k, v)) = row.query {
            req = req.query(&[(k, v)]);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(_) => {
                // Send-leg failure detail is deliberately a fixed string — a
                // reqwest error Display can embed proxy URLs/userinfo.
                return LegFetch::failed("http_send", "send failed (timeout / connect / tls)");
            }
        };
        let status = resp.status();
        if status.is_success() {
            break resp;
        }
        if status.as_u16() == 429 {
            // Runbook `rate_limited`: counted + ONE bounded backoff (the
            // short boot-probe 5s, documented at the constant) — never
            // out-polled.
            metrics::counter!("tv_groww_portfolio_rate_limited_total").increment(1);
            if !rate_limit_retried {
                rate_limit_retried = true;
                tokio::time::sleep(Duration::from_secs(PROBE_RATE_LIMIT_RETRY_SECS)).await;
                continue;
            }
            return LegFetch::failed("rate_limited", "429");
        }
        if status.is_redirection() {
            // Policy::none() client — a redirect is a HARD leg failure
            // (house §18 doctrine: never follow with the bearer attached).
            return LegFetch::failed("redirect", &status.as_u16().to_string());
        }
        return LegFetch::failed("http_status", &status.as_u16().to_string());
    };
    // Bounded STREAMING body read: pre-reject an oversized declared length,
    // then accumulate chunks and ABORT the moment the cap is crossed — peak
    // RAM is bounded by PROBE_MAX_BODY_BYTES, never by the peer (fix B).
    if resp
        .content_length()
        .is_some_and(|len| len > u64::try_from(PROBE_MAX_BODY_BYTES).unwrap_or(u64::MAX))
    {
        return LegFetch::failed("body_too_large", "content-length exceeds cap");
    }
    let mut resp = resp;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len().saturating_add(chunk.len()) > PROBE_MAX_BODY_BYTES {
                    return LegFetch::failed("body_too_large", "body exceeded cap; read aborted");
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => {
                return LegFetch::failed("body_read", "body read failed");
            }
        }
    }
    let body: Value = match serde_json::from_slice(&buf) {
        Ok(v) => v,
        Err(_) => {
            // Fixed detail — a serde error is positional but stay value-free.
            return LegFetch::failed("json_parse", "unparseable JSON body");
        }
    };
    match classify_envelope(&body) {
        EnvelopeOutcome::Success(payload) => {
            let outcome = leg_outcome_from_payload(row.kind, payload);
            let payload = if matches!(outcome, ProbeLegOutcome::Failed { .. }) {
                None
            } else {
                Some(payload.clone()) // APPROVED: cold path — one-shot boot probe, not the tick hot path
            };
            LegFetch { outcome, payload }
        }
        EnvelopeOutcome::Failure { ga_code } => LegFetch {
            outcome: ProbeLegOutcome::Failed {
                stage: "envelope_failure",
                // Sanitized + bounded at the classify_envelope choke point.
                detail: ga_code.unwrap_or_else(|| "GA code absent".to_owned()),
            },
            payload: None,
        },
        EnvelopeOutcome::Malformed => {
            LegFetch::failed("envelope_malformed", "no SUCCESS/FAILURE envelope")
        }
    }
}

/// The per-run leg results + the payloads briefly retained for the row
/// checks / cold-boot labeling (dropped before the report is returned).
struct ProbeLegRun {
    legs: Vec<ProbeLeg>,
    position_payloads: Vec<Value>,
    margin_payload: Option<Value>,
}

/// Drive the plan through a leg fetcher, honoring the per-leg Gate-1 view.
/// Generic over the fetcher so the leg gate is MUTATION-TESTABLE without a
/// network: a test fetcher that panics on a gated leg fails loudly if the
/// `allowed` check is ever dropped (fix D).
async fn run_probe_legs<F, Fut>(
    plan: &[ProbeLegPlan; PROBE_PLAN_LEGS],
    mut fetch_leg: F,
) -> ProbeLegRun
where
    F: FnMut(ProbeLegPlan) -> Fut,
    Fut: Future<Output = LegFetch>,
{
    let mut legs: Vec<ProbeLeg> = Vec::with_capacity(plan.len());
    let mut position_payloads: Vec<Value> = Vec::new();
    let mut margin_payload: Option<Value> = None;
    let mut fired_any = false;
    for row in plan {
        if !row.allowed {
            metrics::counter!(
                "tv_groww_portfolio_fetch_total",
                "leg" => row.leg,
                "outcome" => "skipped_gate"
            )
            .increment(1);
            legs.push(ProbeLeg {
                leg: row.leg,
                outcome: ProbeLegOutcome::SkippedGate {
                    reason: row.gate_reason,
                },
            });
            continue;
        }
        if fired_any {
            tokio::time::sleep(Duration::from_millis(PROBE_INTER_REQUEST_GAP_MS)).await;
        }
        fired_any = true;
        let LegFetch { outcome, payload } = fetch_leg(*row).await;
        metrics::counter!(
            "tv_groww_portfolio_fetch_total",
            "leg" => row.leg,
            "outcome" => leg_outcome_label(&outcome)
        )
        .increment(1);
        match row.kind {
            LegKind::Positions => {
                if let Some(p) = payload {
                    position_payloads.push(p);
                }
            }
            LegKind::Margin => margin_payload = payload,
            LegKind::Holdings => {}
        }
        legs.push(ProbeLeg {
            leg: row.leg,
            outcome,
        });
    }
    ProbeLegRun {
        legs,
        position_payloads,
        margin_payload,
    }
}

/// The one-shot boot FIELD-INVENTORY probe (design §3.1 probe-first posture):
/// positions(CASH) + positions(FNO) + positions(COMMODITY — the N1
/// shared-account blind-spot closure) + holdings + margin, fetched ONCE,
/// sequentially, ≥250ms apart, each 5s-bounded. Field NAMES + types only —
/// never values (redaction discipline). No QuestDB write, no snapshot
/// publish, no Telegram in this PR (6c.3 owns those).
///
/// Gate-1: refuses INERT (zero requests, zero token reads) unless
/// `portfolio_read` AND `probe_and_report` are both true; the margin leg
/// additionally requires `margin_read` (skipped-gate otherwise).
///
/// `fetch_token` is the SSM read-only token provider injected by the app
/// layer (production: the core crate's `fetch_groww_access_token` — the
/// bruteX minter Lambda owns the mint; we ONLY read). Injected as a
/// parameter because `tickvault-core` is deliberately NOT a production
/// dependency of this crate (no new dependency edge), and so gate refusal
/// provably precedes any token read. NOTE (provider contract): the
/// provider's `Err(String)` is LOGGED verbatim at the `token_read` emit
/// site — a provider MUST return fixed, secret-free error text (the
/// production SSM reader does; it never echoes parameter values).
///
/// # Errors
///
/// [`ProbeError::TokenUnavailable`] (bounded transient — re-probe ≥60s /
/// next boot, never day-killing) and [`ProbeError::ClientBuild`]. A single
/// FAILING LEG is a [`ProbeLegOutcome::Failed`] inside the report, not an
/// error; failed legs are coalesced into ONE `cycle_failed` error line per
/// run (runbook §1).
// WIRING-EXEMPT: probe boot entry point — the app supervised-spawn half of plan item 6c.2 wires it (main.rs is build-lead/app-PR surface; this area PR must not touch it).
pub async fn run_portfolio_probe<F, Fut>(
    params: &PortfolioProbeParams,
    fetch_token: F,
) -> Result<ProbeOutcome, ProbeError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<SecretString, String>>,
{
    if let Some(reason) = gate_refusal(params) {
        info!(
            feed = "groww",
            reason, "groww portfolio probe refused inert (Gate 1)"
        );
        return Ok(ProbeOutcome::RefusedInert { reason });
    }

    let token = match fetch_token().await {
        Ok(t) => t,
        Err(e) => {
            error!(
                code = ErrorCode::GrowwPort01SnapshotDegraded.code_str(),
                stage = "token_read",
                feed = "groww",
                error = %e,
                "groww portfolio probe: SSM token read failed — bounded transient, re-probe next window"
            );
            return Err(ProbeError::TokenUnavailable {
                detail: "SSM read failed".to_owned(),
            });
        }
    };

    let client = match build_portfolio_client() {
        Ok(c) => c,
        Err(e) => {
            error!(
                code = ErrorCode::GrowwPort01SnapshotDegraded.code_str(),
                stage = "client_build",
                feed = "groww",
                error = %e,
                "groww portfolio probe: reqwest client build failed — bounded transient"
            );
            return Err(ProbeError::ClientBuild(e));
        }
    };

    let plan = build_probe_plan(params);
    let ProbeLegRun {
        legs,
        position_payloads,
        margin_payload,
    } = run_probe_legs(&plan, |row| fetch_probe_leg(&client, row, &token)).await;

    // ONE coalesced cycle_failed line per probe run (runbook §1) — the
    // per-leg outcomes live in the report + the fetch counters.
    if let Some(failed_legs) = summarize_failed_legs(&legs) {
        error!(
            code = ErrorCode::GrowwPort01SnapshotDegraded.code_str(),
            stage = "cycle_failed",
            feed = "groww",
            failed_legs = %failed_legs,
            "groww portfolio probe: one or more legs failed (coalesced — one line per run)"
        );
    }

    let row_checks = probe_row_checks(&position_payloads);
    let verdict = classify_probe_verdict(&legs);
    let margin_quiet = margin_payload.as_ref().is_some_and(margin_shows_no_usage);
    let ok_empty_cold_boot = matches!(verdict, ProbeVerdict::PassEmpty) && margin_quiet;
    // Payloads (values) drop HERE — only names/types (+ the derived quiet
    // bool) survive into the report.
    drop(position_payloads);
    drop(margin_payload);

    info!(
        feed = "groww",
        verdict = ?verdict,
        legs = legs.len(),
        ok_empty_cold_boot,
        "groww portfolio probe complete (field-inventory only)"
    );
    Ok(ProbeOutcome::Report(ProbeReport {
        legs,
        verdict,
        row_checks,
        ok_empty_cold_boot,
    }))
}

// ---------------------------------------------------------------------------
// Tests — canned serde_json fixtures ONLY; no network anywhere.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Run the pure per-leg pipeline (envelope → binding → inventory) on a
    /// canned envelope body, mirroring the transport fn's post-parse arm.
    fn leg_from_body(leg: &'static str, kind: LegKind, body: &Value) -> (ProbeLeg, Option<Value>) {
        match classify_envelope(body) {
            EnvelopeOutcome::Success(payload) => {
                let outcome = leg_outcome_from_payload(kind, payload);
                let payload = if matches!(outcome, ProbeLegOutcome::Failed { .. }) {
                    None
                } else {
                    Some(payload.clone())
                };
                (ProbeLeg { leg, outcome }, payload)
            }
            EnvelopeOutcome::Failure { ga_code } => (
                ProbeLeg {
                    leg,
                    outcome: ProbeLegOutcome::Failed {
                        stage: "envelope_failure",
                        detail: ga_code.unwrap_or_else(|| "GA code absent".to_owned()),
                    },
                },
                None,
            ),
            EnvelopeOutcome::Malformed => (
                ProbeLeg {
                    leg,
                    outcome: ProbeLegOutcome::Failed {
                        stage: "envelope_malformed",
                        detail: "no SUCCESS/FAILURE envelope".to_owned(),
                    },
                },
                None,
            ),
        }
    }

    fn positions_body(segment: &str) -> Value {
        // Credit/debit legs that deliberately do NOT reconcile to any neat
        // net — PASS must never depend on value-level arithmetic (N9).
        json!({
            "status": "SUCCESS",
            "payload": {
                "positions": [
                    {
                        "trading_symbol": "SENSITIVE-VALUE-XYZ",
                        "segment": segment,
                        "product": "MIS",
                        "credit_quantity": 4242,
                        "credit_price": 123_456,
                        "debit_quantity": 7,
                        "debit_price": 999_999,
                        "isin": "INE002A01018",
                        "realised_pnl": -31_337
                    },
                    {
                        "trading_symbol": "SECOND-ROW-VALUE",
                        "segment": segment,
                        "product": "NRML",
                        "credit_quantity": 1,
                        "debit_quantity": 900_000
                    }
                ]
            }
        })
    }

    fn margin_body() -> Value {
        json!({
            "status": "SUCCESS",
            "payload": {
                "clearing_margin": 0,
                "net_margin_used": 55_555,
                "balance_available": 1_000_000
            }
        })
    }

    fn quiet_margin_body() -> Value {
        json!({
            "status": "SUCCESS",
            "payload": {
                "clear_cash": 1_000_000,
                "net_margin_used": 0,
                "fno_margin_details": { "span_margin_used": 0 },
                "commodity_margin_details": { "commodity_span_margin": 0 }
            }
        })
    }

    #[test]
    fn test_probe_pass_is_field_inventory_only_no_netqty_gate() {
        // Every attempted leg succeeds and rows were observed → PASS, even
        // though the canned credit/debit values reconcile to no sane
        // net_qty and the margin usage contradicts the tiny book. Verdict
        // input = leg outcomes ONLY (design N9 — no value-level
        // cross-check may gate the probe).
        let (cash, _) = leg_from_body(
            "positions_cash",
            LegKind::Positions,
            &positions_body("CASH"),
        );
        let (fno, _) = leg_from_body("positions_fno", LegKind::Positions, &positions_body("FNO"));
        let (margin, _) = leg_from_body("margin", LegKind::Margin, &margin_body());
        let (holdings, _) = leg_from_body(
            "holdings",
            LegKind::Holdings,
            &json!({"status": "SUCCESS", "payload": {"holdings": []}}),
        );
        // The empty holdings book classifies OkEmpty — success, distinct
        // from both Inventory-with-rows and Failed (fix G).
        assert!(matches!(holdings.outcome, ProbeLegOutcome::OkEmpty { .. }));
        let legs = vec![cash, fno, holdings, margin];
        assert_eq!(classify_probe_verdict(&legs), ProbeVerdict::Pass);

        // A SkippedGate margin leg among inventoried ones still PASSES —
        // skipped legs are NOT attempted (hostile review #1 finding 2).
        let mut with_skip = legs.clone();
        with_skip[3] = ProbeLeg {
            leg: "margin",
            outcome: ProbeLegOutcome::SkippedGate {
                reason: "margin_read=false",
            },
        };
        assert_eq!(classify_probe_verdict(&with_skip), ProbeVerdict::Pass);

        // A failed leg among inventoried ones downgrades to PARTIAL; all
        // failed = FAIL; zero attempted = FAIL (fail-closed floor).
        let (bad, _) = leg_from_body("margin", LegKind::Margin, &json!({"nonsense": true}));
        let mut partial = legs.clone();
        partial[3] = bad.clone();
        assert_eq!(classify_probe_verdict(&partial), ProbeVerdict::Partial);
        let all_bad = vec![bad.clone(), bad.clone(), bad.clone(), bad];
        assert_eq!(classify_probe_verdict(&all_bad), ProbeVerdict::Fail);
        assert_eq!(classify_probe_verdict(&[]), ProbeVerdict::Fail);
        let only_skips = vec![ProbeLeg {
            leg: "margin",
            outcome: ProbeLegOutcome::SkippedGate {
                reason: "margin_read=false",
            },
        }];
        assert_eq!(classify_probe_verdict(&only_skips), ProbeVerdict::Fail);
    }

    #[test]
    fn test_probe_verdict_pass_empty_and_cold_boot_labeling() {
        // Every leg succeeds but every row container is EMPTY → PassEmpty,
        // the distinct zero-row verdict that must NOT certify the 6c.5
        // enable flip (fix G; hostile review #3 finding 4).
        let empty_positions = json!({"status": "SUCCESS", "payload": {"positions": []}});
        let (cash, _) = leg_from_body("positions_cash", LegKind::Positions, &empty_positions);
        let (fno, _) = leg_from_body("positions_fno", LegKind::Positions, &empty_positions);
        let (com, _) = leg_from_body("positions_commodity", LegKind::Positions, &empty_positions);
        let (holdings, _) = leg_from_body(
            "holdings",
            LegKind::Holdings,
            &json!({"status": "SUCCESS", "payload": {"holdings": []}}),
        );
        let (margin, margin_payload) =
            leg_from_body("margin", LegKind::Margin, &quiet_margin_body());
        let legs = vec![cash, fno, com, holdings, margin];
        assert_eq!(classify_probe_verdict(&legs), ProbeVerdict::PassEmpty);

        // Margin quiet (all documented usage figures zero, incl. nested)
        // → the ok_empty_cold_boot label input holds.
        assert!(margin_payload.as_ref().is_some_and(margin_shows_no_usage));
        // Margin with usage → NOT quiet (the false-flat contradiction is
        // never labeled a cold boot).
        let busy = margin_body();
        let busy_payload = busy.get("payload").cloned().unwrap_or(Value::Null);
        assert!(!margin_shows_no_usage(&busy_payload));
        // Nested usage is found too.
        let nested = json!({"fno_margin_details": {"span_margin_used": 7}});
        assert!(!margin_shows_no_usage(&nested));
    }

    #[test]
    fn test_probe_row_binding_is_explicit_never_alphabetical_first() {
        // An alphabetically-EARLIER sibling array ("errors" < "positions")
        // must NOT capture the row binding — the documented container key
        // wins (fix E: the NoRows-while-rows-exist hole).
        let body = json!({
            "status": "SUCCESS",
            "payload": {
                "errors": [],
                "positions": [
                    {"trading_symbol": "ROW-1", "segment": "CASH", "product": "MIS"}
                ]
            }
        });
        let (leg, payload) = leg_from_body("positions_cash", LegKind::Positions, &body);
        let ProbeLegOutcome::Inventory { row_count, .. } = leg.outcome else {
            panic!("documented container must bind: {leg:?}"); // APPROVED: test
        };
        assert_eq!(
            row_count,
            Some(1),
            "must bind payload.positions, not payload.errors"
        );
        // The row checks read the SAME binding.
        let payloads = [payload.unwrap_or(Value::Null)];
        let checks = probe_row_checks(&payloads).unwrap_or_else(|| panic!("payload present")); // APPROVED: test
        assert_eq!(checks.segment_field_per_row, FieldPresence::AllRows);
        assert_eq!(checks.contributing_legs, 1);

        // A bare top-level array payload also binds (documented fallback).
        let top_level = json!({"status": "SUCCESS", "payload": [{"segment": "FNO"}]});
        let (leg2, _) = leg_from_body("positions_fno", LegKind::Positions, &top_level);
        assert!(matches!(
            leg2.outcome,
            ProbeLegOutcome::Inventory {
                row_count: Some(1),
                ..
            }
        ));

        // NO documented container + no top-level array → the leg FAILS as
        // envelope_unrecognized — never a silent bind to some other array.
        let weird = json!({"status": "SUCCESS", "payload": {"aaa_rows": [{"x": 1}], "meta": {}}});
        let (leg3, payload3) = leg_from_body("positions_cash", LegKind::Positions, &weird);
        assert!(matches!(
            leg3.outcome,
            ProbeLegOutcome::Failed {
                stage: "envelope_unrecognized",
                ..
            }
        ));
        assert!(payload3.is_none(), "failed legs retain no payload");

        // Margin is not row-bearing: object payload inventories with
        // row_count None (never unrecognized, never OkEmpty).
        let (leg4, _) = leg_from_body("margin", LegKind::Margin, &margin_body());
        assert!(matches!(
            leg4.outcome,
            ProbeLegOutcome::Inventory {
                row_count: None,
                ..
            }
        ));
    }

    #[test]
    fn test_probe_plan_gates_margin_and_covers_all_three_segments() {
        // Pure Gate-1 leg view (fix D): margin_read=false gates EXACTLY the
        // margin leg; the other 4 legs (3 positions segments + holdings)
        // stay unconditional. A mutation dropping the margin gate flips
        // `allowed` and fails here.
        let off = PortfolioProbeParams {
            portfolio_read: true,
            margin_read: false,
            probe_and_report: true,
        };
        let plan = build_probe_plan(&off);
        assert_eq!(plan.len(), PROBE_PLAN_LEGS);
        let leg_names: Vec<&str> = plan.iter().map(|r| r.leg).collect();
        assert_eq!(
            leg_names,
            [
                "positions_cash",
                "positions_fno",
                "positions_commodity",
                "holdings",
                "margin"
            ],
            "fire order + the COMMODITY blind-spot leg (fix H) are pinned"
        );
        for row in &plan {
            if row.leg == "margin" {
                assert!(!row.allowed, "margin leg must be gated by margin_read");
                assert_eq!(row.gate_reason, "margin_read=false");
                assert_eq!(row.kind, LegKind::Margin);
            } else {
                assert!(row.allowed, "{} must be unconditional", row.leg);
            }
        }
        // COMMODITY leg rides the positions endpoint + kind.
        let commodity = plan
            .iter()
            .find(|r| r.leg == "positions_commodity")
            .unwrap_or_else(|| panic!("commodity leg must exist")); // APPROVED: test
        assert_eq!(commodity.url, GROWW_PORTFOLIO_POSITIONS_URL);
        assert_eq!(commodity.query, Some(("segment", "COMMODITY")));
        assert_eq!(commodity.kind, LegKind::Positions);

        // margin_read=true → all 5 allowed.
        let on = PortfolioProbeParams {
            portfolio_read: true,
            margin_read: true,
            probe_and_report: true,
        };
        assert!(build_probe_plan(&on).iter().all(|r| r.allowed));
    }

    #[tokio::test]
    async fn test_probe_leg_runner_never_fetches_a_gated_leg() {
        // Drive the REAL leg runner with a fetcher that PANICS on the
        // margin leg: a mutation dropping the loop's `allowed` check fires
        // the margin GET and fails this test loudly (fix D — the
        // mutation-blind gate hole, hostile review #1 finding 2).
        let params = PortfolioProbeParams {
            portfolio_read: true,
            margin_read: false,
            probe_and_report: true,
        };
        let plan = build_probe_plan(&params);
        let run = run_probe_legs(&plan, |row: ProbeLegPlan| async move {
            assert_ne!(row.leg, "margin", "gated margin leg must NEVER be fetched"); // APPROVED: test
            let payload = match row.kind {
                LegKind::Positions => json!({"positions": []}),
                LegKind::Holdings => json!({"holdings": []}),
                LegKind::Margin => json!({"net_margin_used": 0}),
            };
            LegFetch {
                outcome: leg_outcome_from_payload(row.kind, &payload),
                payload: Some(payload),
            }
        })
        .await;
        assert_eq!(run.legs.len(), PROBE_PLAN_LEGS);
        let margin_leg = run
            .legs
            .iter()
            .find(|l| l.leg == "margin")
            .unwrap_or_else(|| panic!("margin leg must be present in the report")); // APPROVED: test
        assert_eq!(
            margin_leg.outcome,
            ProbeLegOutcome::SkippedGate {
                reason: "margin_read=false"
            },
            "skipped-gate margin leg must be REPORTED, never silently absent"
        );
        assert!(run.margin_payload.is_none());
        assert_eq!(
            run.position_payloads.len(),
            3,
            "all 3 positions segments fetched"
        );
        // All-empty books with margin skipped → PassEmpty, and the
        // cold-boot label must be FALSE (margin quiet unproven).
        assert_eq!(classify_probe_verdict(&run.legs), ProbeVerdict::PassEmpty);
        let margin_quiet = run
            .margin_payload
            .as_ref()
            .is_some_and(margin_shows_no_usage);
        assert!(!margin_quiet, "skipped margin can never label a cold boot");
    }

    #[tokio::test]
    async fn test_probe_token_failure_is_bounded_transient_never_day_killing() {
        // A token-read failure is a typed BOUNDED TRANSIENT (design row 7):
        // the caller re-probes ≥60s / next boot. It is an Err, structurally
        // DISTINCT from the ProbeVerdict::Fail that keeps the pipeline OFF —
        // a transient can never be misread as a schema-inventory FAIL.
        // Drive the REAL entry point with an injected failing provider
        // (zero network — the failure happens before any request).
        let on = PortfolioProbeParams {
            portfolio_read: true,
            margin_read: true,
            probe_and_report: true,
        };
        let token_err = match run_portfolio_probe(&on, || async {
            Err::<SecretString, String>("ssm unreachable".to_owned())
        })
        .await
        {
            Err(e @ ProbeError::TokenUnavailable { .. }) => e,
            other => panic!("expected TokenUnavailable, got {other:?}"), // APPROVED: test
        };
        assert!(token_err.is_bounded_transient());
        let build_err = ProbeError::ClientBuild(PortfolioClientBuildError {
            message: "resolver init failed".to_owned(),
        });
        assert!(build_err.is_bounded_transient());
        // No secret echo in the rendered forms.
        let rendered = format!("{token_err} / {build_err}");
        assert!(rendered.contains("token unavailable"));
        assert!(!rendered.to_lowercase().contains("bearer"));
    }

    #[test]
    fn test_probe_capture_redacts_values_names_only() {
        // The inventory (the ONLY thing the report keeps from a payload)
        // carries field names + JSON types — never a wire value.
        let body = positions_body("CASH");
        let (leg, _) = leg_from_body("positions_cash", LegKind::Positions, &body);
        let ProbeLegOutcome::Inventory {
            ref fields,
            row_count,
            inventory_truncated,
        } = leg.outcome
        else {
            panic!("fixture must inventory"); // APPROVED: test
        };
        assert_eq!(row_count, Some(2));
        assert!(!inventory_truncated, "small fixture must not truncate");
        let dump = format!("{fields:?}");
        // Names present.
        for name in [
            "payload.positions",
            "payload.positions[].trading_symbol",
            "payload.positions[].credit_quantity",
            "payload.positions[].isin",
        ] {
            assert!(dump.contains(name), "missing field name {name}: {dump}");
        }
        // Values absent — the redaction contract.
        for value in [
            "SENSITIVE-VALUE-XYZ",
            "INE002A01018",
            "4242",
            "31337",
            "-31_337",
        ] {
            assert!(!dump.contains(value), "VALUE LEAKED: {value} in {dump}");
        }
        // The whole-report debug dump is equally value-free.
        let report = ProbeReport {
            legs: vec![leg],
            verdict: ProbeVerdict::Pass,
            row_checks: None,
            ok_empty_cold_boot: false,
        };
        let report_dump = format!("{report:?}");
        assert!(!report_dump.contains("SENSITIVE-VALUE-XYZ"));
        assert!(!report_dump.contains("INE002A01018"));
    }

    #[test]
    fn test_probe_inventory_sanitizes_paths_and_flags_truncation() {
        // Hostile object keys (control chars, BiDi override, huge length)
        // are sanitized + bounded at inventory time (fix C — the
        // sanitize_audit_string / FEED-REJECT-01 precedent).
        let evil_key = format!("evil\u{202e}\n{}", "K".repeat(500));
        let payload = json!({ evil_key: 1, "ok_field": 2 });
        let (fields, truncated) = field_inventory(&payload);
        assert!(!truncated);
        let dump = format!("{fields:?}");
        assert!(!dump.contains('\u{202e}'), "BiDi override must be stripped");
        assert!(!dump.contains("\\n"), "control chars must be stripped");
        for entry in &fields {
            assert!(
                entry.path.chars().count() <= INVENTORY_PATH_MAX_CHARS + 1, // +1 for the … marker
                "path over cap: {}",
                entry.path
            );
        }
        assert!(dump.contains("ok_field"));

        // Entry-cap overflow is flagged honestly, never silent.
        let mut wide = serde_json::Map::new();
        for i in 0..(INVENTORY_MAX_ENTRIES + 10) {
            wide.insert(format!("f{i}"), json!(1));
        }
        let (capped, overflowed) = field_inventory(&Value::Object(wide));
        assert_eq!(capped.len(), INVENTORY_MAX_ENTRIES);
        assert!(
            overflowed,
            "entry-cap overflow must set inventory_truncated"
        );

        // Depth-cap overflow is flagged too.
        let deep = json!({"a":{"b":{"c":{"d":{"e":{"f":{"g":{"h":{"i":{"j": 1}}}}}}}}}});
        let (_, deep_truncated) = field_inventory(&deep);
        assert!(
            deep_truncated,
            "depth-cap overflow must set inventory_truncated"
        );
    }

    #[test]
    fn test_probe_checks_segment_required_and_product_per_row() {
        // Checklist (i): per-row `segment` field presence; (ii): per-row
        // `product` presence — computed over the combined rows of the
        // contributing positions legs.
        let cash = positions_body("CASH");
        let cash_payload = cash.get("payload").cloned().unwrap_or(Value::Null);
        // FNO fixture: one row missing `product` → SomeRows.
        let fno = json!({
            "positions": [
                {"trading_symbol": "F1", "segment": "FNO", "product": "NRML"},
                {"trading_symbol": "F2", "segment": "FNO"}
            ]
        });
        let checks =
            probe_row_checks(&[cash_payload, fno]).unwrap_or_else(|| panic!("payloads present")); // APPROVED: test
        assert_eq!(checks.segment_field_per_row, FieldPresence::AllRows);
        assert_eq!(checks.product_field_per_row, FieldPresence::SomeRows);
        assert_eq!(checks.contributing_legs, 2);

        // Empty books: presence UNKNOWN, stated as NoRows — never Absent.
        let empty = json!({"positions": []});
        let checks_empty =
            probe_row_checks(&[empty.clone(), empty]).unwrap_or_else(|| panic!("payloads present")); // APPROVED: test
        assert_eq!(checks_empty.segment_field_per_row, FieldPresence::NoRows);
        assert_eq!(checks_empty.product_field_per_row, FieldPresence::NoRows);

        // No positions payload at all → no checks (honest None, not a guess).
        assert!(probe_row_checks(&[]).is_none());

        // Product absent on every row → Absent (distinct from NoRows).
        let no_product = json!({"positions": [{"trading_symbol": "X", "segment": "CASH"}]});
        let checks_absent =
            probe_row_checks(&[no_product]).unwrap_or_else(|| panic!("payload present")); // APPROVED: test
        assert_eq!(checks_absent.product_field_per_row, FieldPresence::Absent);
        assert_eq!(checks_absent.segment_field_per_row, FieldPresence::AllRows);
        assert_eq!(checks_absent.contributing_legs, 1);
    }

    #[tokio::test]
    async fn test_probe_gate1_refusal_is_inert_when_flags_false() {
        // Gate refusal must precede ANY token read: the injected provider
        // panics if invoked, so a regression that fetches before gating
        // fails this test loudly.
        let panicking_provider = || async {
            panic!("gate refusal must precede the token read"); // APPROVED: test
            #[allow(unreachable_code)]
            // APPROVED: test-only type anchor for the never-invoked provider
            Err::<SecretString, String>(String::new())
        };
        // portfolio_read=false → RefusedInert (zero requests, zero SSM).
        let off = PortfolioProbeParams {
            portfolio_read: false,
            margin_read: true,
            probe_and_report: true,
        };
        match run_portfolio_probe(&off, panicking_provider).await {
            Ok(ProbeOutcome::RefusedInert { reason }) => {
                assert_eq!(reason, "portfolio_read=false");
            }
            other => panic!("expected inert refusal, got {other:?}"), // APPROVED: test
        }
        // probe_and_report=false → RefusedInert too.
        let no_probe = PortfolioProbeParams {
            portfolio_read: true,
            margin_read: true,
            probe_and_report: false,
        };
        match run_portfolio_probe(&no_probe, panicking_provider).await {
            Ok(ProbeOutcome::RefusedInert { reason }) => {
                assert_eq!(reason, "probe_and_report=false");
            }
            other => panic!("expected inert refusal, got {other:?}"), // APPROVED: test
        }
        // Pure-gate parity: gate_refusal mirrors the entry behaviour.
        assert_eq!(gate_refusal(&off), Some("portfolio_read=false"));
        assert_eq!(gate_refusal(&no_probe), Some("probe_and_report=false"));
        let on = PortfolioProbeParams {
            portfolio_read: true,
            margin_read: false,
            probe_and_report: true,
        };
        assert_eq!(gate_refusal(&on), None);
    }

    #[test]
    fn test_endpoint_consts_pinned_and_envelope_classifier_total() {
        // The 4 documented portfolio GETs (Gate-5 exemption: this file only).
        assert_eq!(
            GROWW_PORTFOLIO_POSITIONS_URL,
            "https://api.groww.in/v1/positions/user"
        );
        assert_eq!(
            GROWW_PORTFOLIO_POSITION_FOR_SYMBOL_URL,
            "https://api.groww.in/v1/positions/trading-symbol"
        );
        assert_eq!(
            GROWW_PORTFOLIO_HOLDINGS_URL,
            "https://api.groww.in/v1/holdings/user"
        );
        assert_eq!(
            GROWW_PORTFOLIO_MARGIN_URL,
            "https://api.groww.in/v1/margins/detail/user"
        );
        // Envelope classifier is total: FAILURE carries the GA code (a code,
        // never a value); garbage classifies Malformed, never panics.
        let failure = json!({"status": "FAILURE", "error": {"code": "GA001", "message": "x"}});
        match classify_envelope(&failure) {
            EnvelopeOutcome::Failure { ga_code } => assert_eq!(ga_code.as_deref(), Some("GA001")),
            _ => panic!("FAILURE envelope must classify Failure"), // APPROVED: test
        }
        for garbage in [
            json!(null),
            json!([1, 2]),
            json!({"status": "WEIRD"}),
            json!({"status": "SUCCESS"}),
            // payload:null must NOT classify Success (fix G).
            json!({"status": "SUCCESS", "payload": null}),
            json!({"payload": {}}),
        ] {
            assert!(matches!(
                classify_envelope(&garbage),
                EnvelopeOutcome::Malformed
            ));
        }
        // Userinfo redaction on client-build errors (no proxy-cred echo) —
        // including an unencoded `/` inside the password (hostile review
        // #2 finding 6).
        let redacted = redact_userinfo("proxy https://user:s3cret@corp:8080 refused");
        assert!(redacted.contains("***@corp"));
        assert!(!redacted.contains("s3cret"));
        let slashy = redact_userinfo("dial https://user:pa/ss@host failed");
        assert!(
            !slashy.contains("pa/ss"),
            "slash-split secret leaked: {slashy}"
        );
        assert!(slashy.contains("***@host"));
    }

    #[test]
    fn test_probe_ga_code_sanitizer_bounds_hostile_body_codes() {
        // The GA code is BODY-derived text (hostile review #2 finding 2):
        // only short [A-Za-z0-9_-] codes survive; anything hostile is
        // replaced WHOLESALE — never a partial echo.
        assert_eq!(sanitize_ga_code("GA001"), "GA001");
        assert_eq!(sanitize_ga_code("rate-limit_42"), "rate-limit_42");
        for hostile in [
            String::new(),
            "x".repeat(GA_CODE_MAX_CHARS + 1),
            "evil\ncode".to_owned(),
            "bidi\u{202e}GA".to_owned(),
            "fake=field injection".to_owned(),
            "two words".to_owned(),
        ] {
            assert_eq!(
                sanitize_ga_code(&hostile),
                "GA code malformed",
                "hostile GA code must be replaced wholesale"
            );
        }
        // The choke point is classify_envelope itself — a hostile FAILURE
        // envelope can never carry raw bytes into Failed.detail.
        let evil = json!({"status": "FAILURE", "error": {"code": "X".repeat(100_000)}});
        match classify_envelope(&evil) {
            EnvelopeOutcome::Failure { ga_code } => {
                assert_eq!(ga_code.as_deref(), Some("GA code malformed"));
            }
            _ => panic!("FAILURE envelope must classify Failure"), // APPROVED: test
        }
        // The coalesced summary is bounded even so.
        let legs = vec![ProbeLeg {
            leg: "positions_cash",
            outcome: ProbeLegOutcome::Failed {
                stage: "envelope_failure",
                detail: "GA code malformed".to_owned(),
            },
        }];
        let summary = summarize_failed_legs(&legs).unwrap_or_default();
        assert!(summary.chars().count() <= FAILED_SUMMARY_MAX_CHARS);
        assert!(summary.contains("positions_cash:envelope_failure"));
        assert!(summarize_failed_legs(&[]).is_none());
    }

    #[test]
    fn test_ratchet_client_hardening_no_redirect_streaming_body_cap() {
        // Source-scan ratchet over the PRODUCTION region only (split at the
        // test-module gate — the AGGREGATOR-SEAL-01 §(c) precedent, so the
        // needles below can never self-match).
        let src = include_str!("portfolio.rs");
        let (prod, _) = src.split_once("#[cfg(test)]").unwrap_or((src, ""));
        assert!(
            prod.contains("redirect(reqwest::redirect::Policy::none())"),
            "the probe client must refuse redirects (house §18 doctrine — bearer-carrying request)"
        );
        assert!(
            prod.contains("body_too_large"),
            "the streaming body cap failure class must exist"
        );
        assert!(
            prod.contains(".chunk().await"),
            "the body must be read incrementally via chunk()"
        );
        assert!(
            !prod.contains(".bytes().await"),
            "post-hoc whole-body buffering is banned — stream with the incremental cap"
        );
        assert!(
            prod.contains("\"rate_limited\""),
            "the 429 leg failure class must exist (runbook parity)"
        );
        assert!(
            prod.contains("tv_groww_portfolio_fetch_total")
                && prod.contains("tv_groww_portfolio_rate_limited_total"),
            "the runbook §5 probe counters must be emitted"
        );
    }
}
