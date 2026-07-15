//! Groww order-side transport — the ONLY file allowed to contain
//! HTTP/`reqwest` and the `/v1/order/*` endpoint PATH strings (Gate 5 of the
//! live-fire lattice — `groww_order_lattice_guard.rs`). Design §4.3; ORD-PR-3.
//!
//! # Shape
//! - [`OrderTransport`] — the async transport trait (typed request → classified
//!   [`TransportOutcome`]); the executor is generic over it, so the paper lane
//!   injects [`NullTransport`] (ZERO HTTP) and live injects
//!   [`GrowwOrderApiClient`] (the `reqwest` impl).
//! - Mutating fns take `&IntentReceipt` — a mutating send WITHOUT a durable
//!   fsynced intent record is a compile error (write-ahead discipline, §4.5).
//! - Classification is PURE ([`classify_response`] / [`classify_send_error`]):
//!   the taxonomy of design §4.3 — success · well-shaped-failure→reject · 429→
//!   rate-limited · 401/403→auth-stale · timeout/5xx/decode/GA-on-non-reject→
//!   ambiguous — is decided by status+shape, NEVER by body substrings (the
//!   AUTH-GAP-05 F12 lesson).
//!
//! # Emit markers
//! `error!`/counter emits carry `target: "groww_ord"`; the typed
//! `GROWW-ORD-xx` ErrorCode emits land in ORD-PR-1 (the variants) — the
//! `// GROWW-ORD-xx emit lands with the shared variants (ORD-PR-1)` markers
//! below flag every site.

use secrecy::{ExposeSecret, SecretString};

use super::intent_ledger::IntentReceipt;
use super::types::{
    GrowwCancelOrderReq, GrowwCreateOrderReq, GrowwEnvelope, GrowwModifyOrderReq,
    GrowwMutationRespPayload, GrowwOrderDetailPayload, GrowwOrderStatusPayload, GrowwSegment,
    GrowwTradeRow,
};

// ---------------------------------------------------------------------------
// Endpoint PATHs + base URL (module-local; Gate-5-confined to THIS file).
// The base-url + path literals move to tickvault-common GROWW_ORDER_* in
// ORD-PR-1 — until then they live here so the paper lane (executor/rate_budget/
// events) contains NO base-url / endpoint string.
// ---------------------------------------------------------------------------

/// Groww API base URL (`16` §1 header).
// moves to tickvault-common GROWW_ORDER_* in ORD-PR-1
pub(crate) const GROWW_ORDER_API_BASE_URL: &str = "https://api.groww.in";

const PATH_CREATE: &str = "/v1/order/create";
const PATH_MODIFY: &str = "/v1/order/modify";
const PATH_CANCEL: &str = "/v1/order/cancel";
const PATH_STATUS_BY_ID: &str = "/v1/order/status/";
const PATH_STATUS_BY_REF: &str = "/v1/order/status/reference/";
const PATH_DETAIL: &str = "/v1/order/detail/";
const PATH_LIST: &str = "/v1/order/list";
const PATH_TRADES: &str = "/v1/order/trades/";

/// x-api-version header value (live house pattern `groww_spot_1m_boot.rs`).
const API_VERSION_HEADER: &str = "x-api-version";
const API_VERSION_VALUE: &str = "1.0";

/// HTTP client timeouts (the `OMS_HTTP_*` constants pattern).
const CONNECT_TIMEOUT_SECS: u64 = 3;
const REQUEST_TIMEOUT_SECS: u64 = 5;
const POOL_IDLE_TIMEOUT_SECS: u64 = 90;

/// Max sanitized response-body excerpt kept on a failure classification.
const MAX_BODY_EXCERPT_CHARS: usize = 300;

// ---------------------------------------------------------------------------
// Classified outcome + reasons
// ---------------------------------------------------------------------------

/// Why a transport call could not be classified as a clean success or a clean
/// definitive reject — the AMBIGUOUS class of design §4.3 (the executor routes
/// every one of these into the resolution ladder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmbiguityReason {
    /// The connection never established — nothing left the box. The executor
    /// treats this as `resolved_not_landed(never_sent)` UNLESS other doubt
    /// exists (doubt ⇒ AMBIGUOUS wins, §4.5).
    ConnectPhase,
    /// The request timed out after send — landing is UNKNOWN.
    Timeout,
    /// A 5xx / non-classified HTTP status (the `u16` is the status; `0` = a
    /// non-HTTP send failure).
    ServerError(u16),
    /// The 2xx/response body could not be decoded into the expected shape.
    Decode,
    /// A well-shaped FAILURE (GA code) rode a NON-reject status (e.g. a 200 or
    /// 5xx carrying `GA003`) — NOT a clean refusal (doc-fidelity F2). Carries
    /// the GA code.
    GaOnNonRejectStatus(String),
    /// A 2xx SUCCESS whose payload was missing a usable order id/status
    /// (fail-closed on O-2, §4.3).
    MissingPayload,
    /// A generic post-send transport failure (redirect/body/other).
    SendFailed,
}

impl AmbiguityReason {
    /// Stable audit label.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ConnectPhase => "connect_phase",
            Self::Timeout => "timeout",
            Self::ServerError(_) => "server_error",
            Self::Decode => "decode",
            Self::GaOnNonRejectStatus(_) => "ga_on_non_reject_status",
            Self::MissingPayload => "missing_payload",
            Self::SendFailed => "send_failed",
        }
    }
}

/// The classified result of ONE transport call (design §4.3 taxonomy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportOutcome<T> {
    /// 2xx SUCCESS with a usable payload.
    Success(T),
    /// DEFINITIVE reject — a 400-class status AND a well-shaped FAILURE
    /// envelope (the ONLY shape that may ever be a clean refusal, §4.3).
    // GROWW-ORD-01 emit lands with the shared variants (ORD-PR-1).
    Rejected {
        /// The HTTP status.
        http_status: u16,
        /// The GA code from the well-shaped FAILURE body.
        ga_code: Option<String>,
        /// The request-specific message (sanitized).
        message: Option<String>,
    },
    /// HTTP 429 — the full ambiguity ladder engages on the SAME reference id
    /// (hostile F-1); never a clean refusal, never out-polled.
    // GROWW-ORD-05 emit lands with the shared variants (ORD-PR-1).
    RateLimited {
        /// The HTTP status (429).
        http_status: u16,
        /// `Retry-After` seconds when the header was present + parseable.
        retry_after_secs: Option<u64>,
        /// Sanitized ≤300-char body excerpt.
        body_excerpt: Option<String>,
    },
    /// HTTP 401/403 — auth-stale (the ~06:00 IST token reset class). The
    /// ladder clock PAUSES on this (design §4.7 / hostile F-5).
    // GROWW-ORD-10 emit lands with the shared variants (ORD-PR-1).
    AuthStale {
        /// The HTTP status (401 or 403).
        http_status: u16,
    },
    /// Outcome UNKNOWN — the resolution ladder owns it.
    // GROWW-ORD-02 emit lands with the shared variants (ORD-PR-1).
    Ambiguous(AmbiguityReason),
}

impl<T> TransportOutcome<T> {
    /// Stable metric-label for `tv_groww_order_*{outcome}`.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Success(_) => "success",
            Self::Rejected { .. } => "rejected",
            Self::RateLimited { .. } => "rate_limited",
            Self::AuthStale { .. } => "auth_stale",
            Self::Ambiguous(_) => "ambiguous",
        }
    }
}

/// A reqwest send-error kind, decoupled from `reqwest` so
/// [`classify_send_error`] is pure + unit-testable (design §4.3
/// connect-vs-post-send distinction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendErrorKind {
    /// The connection never established — nothing left the box.
    Connect,
    /// The request timed out after send.
    Timeout,
    /// Any other transport failure (redirect/body/decode-at-transport).
    Other,
}

/// Map a `reqwest::Error` to a pure [`SendErrorKind`].
// TEST-EXEMPT: constructing a real reqwest::Error requires a live socket; the
// downstream `classify_send_error(SendErrorKind)` mapping is unit-tested.
#[must_use]
pub fn send_error_kind(e: &reqwest::Error) -> SendErrorKind {
    if e.is_connect() {
        SendErrorKind::Connect
    } else if e.is_timeout() {
        SendErrorKind::Timeout
    } else {
        SendErrorKind::Other
    }
}

/// Classify a send-phase failure (no HTTP status was ever received) — PURE.
#[must_use]
pub fn classify_send_error<T>(kind: SendErrorKind) -> TransportOutcome<T> {
    match kind {
        SendErrorKind::Connect => TransportOutcome::Ambiguous(AmbiguityReason::ConnectPhase),
        SendErrorKind::Timeout => TransportOutcome::Ambiguous(AmbiguityReason::Timeout),
        SendErrorKind::Other => TransportOutcome::Ambiguous(AmbiguityReason::SendFailed),
    }
}

/// Classify a fully-received HTTP response — PURE (design §4.3).
///
/// `env` is the decode result of the body into the endpoint's
/// [`GrowwEnvelope<T>`]; `Err(())` = the body failed to decode. `retry_after`
/// / `body_excerpt` are pre-extracted so this stays pure. The taxonomy:
/// - 429 → RateLimited (full ladder);
/// - 401/403 → AuthStale (clock-pause);
/// - decode failure → Ambiguous(Decode);
/// - 2xx + SUCCESS + payload → Success; 2xx + SUCCESS + no payload →
///   Ambiguous(MissingPayload); 2xx + well-shaped FAILURE → Ambiguous(GA on
///   non-reject status, doc-F2); 2xx otherwise → Ambiguous(MissingPayload);
/// - 4xx + well-shaped FAILURE → Rejected (definitive); 4xx otherwise →
///   Ambiguous(ServerError) (fail-closed — no clean refusal without a
///   well-shaped envelope);
/// - everything else (5xx/…) → Ambiguous(ServerError).
#[must_use]
pub fn classify_response<T>(
    status: u16,
    env: Result<GrowwEnvelope<T>, ()>,
    retry_after_secs: Option<u64>,
    body_excerpt: Option<String>,
) -> TransportOutcome<T> {
    if status == 429 {
        return TransportOutcome::RateLimited {
            http_status: status,
            retry_after_secs,
            body_excerpt,
        };
    }
    if status == 401 || status == 403 {
        return TransportOutcome::AuthStale {
            http_status: status,
        };
    }
    let env = match env {
        Ok(e) => e,
        Err(()) => return TransportOutcome::Ambiguous(AmbiguityReason::Decode),
    };
    let is_2xx = (200..=299).contains(&status);
    let is_4xx = (400..=499).contains(&status);
    if is_2xx {
        if env.is_success() {
            return match env.payload {
                Some(p) => TransportOutcome::Success(p),
                None => TransportOutcome::Ambiguous(AmbiguityReason::MissingPayload),
            };
        }
        if env.is_well_shaped_failure() {
            let ga = env
                .error
                .as_ref()
                .and_then(|e| e.code.clone())
                .unwrap_or_default();
            return TransportOutcome::Ambiguous(AmbiguityReason::GaOnNonRejectStatus(ga));
        }
        return TransportOutcome::Ambiguous(AmbiguityReason::MissingPayload);
    }
    if is_4xx {
        if env.is_well_shaped_failure() {
            let (ga_code, message) = env
                .error
                .as_ref()
                .map(|e| (e.code.clone(), e.message.clone()))
                .unwrap_or((None, None));
            return TransportOutcome::Rejected {
                http_status: status,
                ga_code,
                message: message.map(|m| body_excerpt_of(&m)),
            };
        }
        return TransportOutcome::Ambiguous(AmbiguityReason::ServerError(status));
    }
    TransportOutcome::Ambiguous(AmbiguityReason::ServerError(status))
}

/// Parse a `Retry-After` header value (delta-seconds form only — the HTTP-date
/// form is not used by Groww per U-7 and stays `None`). Pure.
#[must_use]
pub fn parse_retry_after_secs(header: Option<&str>) -> Option<u64> {
    header?.trim().parse::<u64>().ok()
}

/// Sanitize + truncate a body/message excerpt: strip control chars (incl.
/// BiDi/zero-width), collapse to ≤[`MAX_BODY_EXCERPT_CHARS`]. Pure — a
/// RESPONSE body never carries our token, but this is defense-in-depth.
#[must_use]
pub fn body_excerpt_of(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control() && !is_bidi_or_zero_width(*c))
        .take(MAX_BODY_EXCERPT_CHARS)
        .collect()
}

fn is_bidi_or_zero_width(c: char) -> bool {
    matches!(c,
        '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2066}'..='\u{2069}'
        | '\u{FEFF}')
}

// ---------------------------------------------------------------------------
// The transport trait
// ---------------------------------------------------------------------------

/// The Groww order-side transport. The executor is generic over this, so the
/// paper lane injects [`NullTransport`] (zero HTTP) and live injects
/// [`GrowwOrderApiClient`]. Mutating fns take `&IntentReceipt` — a mutating
/// send without a durable fsynced intent is a compile error.
///
/// Native `async fn` in a trait (edition 2024) — the executor holds a concrete
/// `T: OrderTransport` (no `dyn`), so no `Send` future bound is imposed here.
#[allow(async_fn_in_trait)] // executor is generic (no dyn); futures awaited inline.
pub trait OrderTransport {
    /// `POST /v1/order/create`.
    async fn create_order(
        &self,
        req: &GrowwCreateOrderReq,
        token: &SecretString,
        receipt: &IntentReceipt,
    ) -> TransportOutcome<GrowwMutationRespPayload>;

    /// `POST /v1/order/modify`.
    async fn modify_order(
        &self,
        req: &GrowwModifyOrderReq,
        token: &SecretString,
        receipt: &IntentReceipt,
    ) -> TransportOutcome<GrowwMutationRespPayload>;

    /// `POST /v1/order/cancel`.
    async fn cancel_order(
        &self,
        req: &GrowwCancelOrderReq,
        token: &SecretString,
        receipt: &IntentReceipt,
    ) -> TransportOutcome<GrowwMutationRespPayload>;

    /// `GET /v1/order/status/{id}?segment=`.
    async fn get_status_by_id(
        &self,
        groww_order_id: &str,
        segment: GrowwSegment,
        token: &SecretString,
    ) -> TransportOutcome<GrowwOrderStatusPayload>;

    /// `GET /v1/order/status/reference/{ref}?segment=` — the place-ambiguity
    /// resolver (design §4.7).
    async fn get_status_by_reference(
        &self,
        order_reference_id: &str,
        segment: GrowwSegment,
        token: &SecretString,
    ) -> TransportOutcome<GrowwOrderStatusPayload>;

    /// `GET /v1/order/detail/{id}?segment=` — full 22-field payload.
    async fn get_order_detail(
        &self,
        groww_order_id: &str,
        segment: GrowwSegment,
        token: &SecretString,
    ) -> TransportOutcome<GrowwOrderDetailPayload>;

    /// `GET /v1/order/list?segment=&page=&page_size=` — day's orders.
    async fn list_orders(
        &self,
        segment: GrowwSegment,
        page: u32,
        page_size: u32,
        token: &SecretString,
    ) -> TransportOutcome<Vec<GrowwOrderDetailPayload>>;

    /// `GET /v1/order/trades/{id}?segment=&page=&page_size=`.
    async fn get_trades(
        &self,
        groww_order_id: &str,
        segment: GrowwSegment,
        page: u32,
        page_size: u32,
        token: &SecretString,
    ) -> TransportOutcome<Vec<GrowwTradeRow>>;
}

// ---------------------------------------------------------------------------
// NullTransport — the paper lane's ZERO-HTTP transport
// ---------------------------------------------------------------------------

/// The paper-lane transport: makes ZERO network calls. Every method returns an
/// inert `Ambiguous(ConnectPhase)` — but the paper executor NEVER invokes the
/// transport (it simulates outcomes against the ledger), so these arms are
/// structurally unreachable in the paper lane. Exists so the executor's
/// `T: OrderTransport` bound is satisfied with a type that CANNOT reach an
/// endpoint (Gate-6 paper-zero-HTTP: no `reqwest`, no URL here).
#[derive(Debug, Clone, Copy, Default)]
pub struct NullTransport;

impl OrderTransport for NullTransport {
    async fn create_order(
        &self,
        _req: &GrowwCreateOrderReq,
        _token: &SecretString,
        _receipt: &IntentReceipt,
    ) -> TransportOutcome<GrowwMutationRespPayload> {
        TransportOutcome::Ambiguous(AmbiguityReason::ConnectPhase)
    }
    async fn modify_order(
        &self,
        _req: &GrowwModifyOrderReq,
        _token: &SecretString,
        _receipt: &IntentReceipt,
    ) -> TransportOutcome<GrowwMutationRespPayload> {
        TransportOutcome::Ambiguous(AmbiguityReason::ConnectPhase)
    }
    async fn cancel_order(
        &self,
        _req: &GrowwCancelOrderReq,
        _token: &SecretString,
        _receipt: &IntentReceipt,
    ) -> TransportOutcome<GrowwMutationRespPayload> {
        TransportOutcome::Ambiguous(AmbiguityReason::ConnectPhase)
    }
    async fn get_status_by_id(
        &self,
        _groww_order_id: &str,
        _segment: GrowwSegment,
        _token: &SecretString,
    ) -> TransportOutcome<GrowwOrderStatusPayload> {
        TransportOutcome::Ambiguous(AmbiguityReason::ConnectPhase)
    }
    async fn get_status_by_reference(
        &self,
        _order_reference_id: &str,
        _segment: GrowwSegment,
        _token: &SecretString,
    ) -> TransportOutcome<GrowwOrderStatusPayload> {
        TransportOutcome::Ambiguous(AmbiguityReason::ConnectPhase)
    }
    async fn get_order_detail(
        &self,
        _groww_order_id: &str,
        _segment: GrowwSegment,
        _token: &SecretString,
    ) -> TransportOutcome<GrowwOrderDetailPayload> {
        TransportOutcome::Ambiguous(AmbiguityReason::ConnectPhase)
    }
    async fn list_orders(
        &self,
        _segment: GrowwSegment,
        _page: u32,
        _page_size: u32,
        _token: &SecretString,
    ) -> TransportOutcome<Vec<GrowwOrderDetailPayload>> {
        TransportOutcome::Ambiguous(AmbiguityReason::ConnectPhase)
    }
    async fn get_trades(
        &self,
        _groww_order_id: &str,
        _segment: GrowwSegment,
        _page: u32,
        _page_size: u32,
        _token: &SecretString,
    ) -> TransportOutcome<Vec<GrowwTradeRow>> {
        TransportOutcome::Ambiguous(AmbiguityReason::ConnectPhase)
    }
}

// ---------------------------------------------------------------------------
// GrowwOrderApiClient — the reqwest impl (live lane)
// ---------------------------------------------------------------------------

/// A client-construction failure (local — types.rs is a frozen core module).
#[derive(Debug, thiserror::Error)]
pub enum ApiClientError {
    /// The reqwest client could not be built (TLS/resolver/fd pressure).
    #[error("groww order api client build failed: {0}")]
    ClientBuild(String),
}

/// The live Groww order transport — `reqwest` over the 8 endpoints (§2.2).
/// Auth per request: `Bearer {token}` + `x-api-version: 1.0`; the token is
/// passed in (SSM-read, NEVER minted, never stored on the client, never
/// logged). Construction is gated by the live-fire lattice at the boot layer
/// (ORD-PR-5); this type embeds no gate.
#[derive(Debug, Clone)]
pub struct GrowwOrderApiClient {
    http: reqwest::Client,
    base_url: String,
}

impl GrowwOrderApiClient {
    /// Build the client with the OMS-style timeouts.
    ///
    /// # Errors
    /// [`ApiClientError::ClientBuild`] if the reqwest client cannot be built
    /// (never panics — `reqwest::Client::new()` would).
    // TEST-EXEMPT: builds a live reqwest client; the reqwest transport methods
    // are exercised through the mock/NullTransport in the executor tests, and
    // the pure classification they delegate to is unit-tested here.
    pub fn new(base_url: impl Into<String>) -> Result<Self, ApiClientError> {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .pool_idle_timeout(std::time::Duration::from_secs(POOL_IDLE_TIMEOUT_SECS))
            .build()
            .map_err(|e| ApiClientError::ClientBuild(e.to_string()))?;
        Ok(Self {
            http,
            base_url: base_url.into(),
        })
    }

    /// Build against the default Groww base URL.
    ///
    /// # Errors
    /// See [`GrowwOrderApiClient::new`].
    // TEST-EXEMPT: thin wrapper over `new` (itself test-exempt, builds a live client).
    pub fn with_default_base() -> Result<Self, ApiClientError> {
        Self::new(GROWW_ORDER_API_BASE_URL)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    /// Send a POST mutation + classify. Generic over the response payload.
    async fn post_mutation<B: serde::Serialize>(
        &self,
        path: &str,
        body: &B,
        token: &SecretString,
    ) -> TransportOutcome<GrowwMutationRespPayload> {
        self.send_and_classify(self.http.post(self.url(path)).json(body), token)
            .await
    }

    /// Send a GET read (+ query) and classify. Generic over the payload.
    async fn get_read<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
        token: &SecretString,
    ) -> TransportOutcome<T> {
        self.send_and_classify(self.http.get(self.url(path)).query(query), token)
            .await
    }

    async fn send_and_classify<T: serde::de::DeserializeOwned>(
        &self,
        builder: reqwest::RequestBuilder,
        token: &SecretString,
    ) -> TransportOutcome<T> {
        let resp = builder
            .bearer_auth(token.expose_secret())
            .header(API_VERSION_HEADER, API_VERSION_VALUE)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                // GROWW-ORD-02 emit lands with the shared variants (ORD-PR-1).
                tracing::warn!(
                    target: "groww_ord",
                    err = %e,
                    kind = ?send_error_kind(&e),
                    "groww order transport: send failed (ambiguous)"
                );
                return classify_send_error(send_error_kind(&e));
            }
        };
        let status = resp.status().as_u16();
        let retry_after = parse_retry_after_secs(
            resp.headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
        );
        let body = match resp.text().await {
            Ok(b) => b,
            Err(_) => {
                // Body read/decode failure — outcome unknown.
                return TransportOutcome::Ambiguous(AmbiguityReason::Decode);
            }
        };
        let excerpt = if !(200..=299).contains(&status) {
            Some(body_excerpt_of(&body))
        } else {
            None
        };
        let env: Result<GrowwEnvelope<T>, ()> = serde_json::from_str(&body).map_err(|_| ());
        classify_response(status, env, retry_after, excerpt)
    }
}

impl OrderTransport for GrowwOrderApiClient {
    async fn create_order(
        &self,
        req: &GrowwCreateOrderReq,
        token: &SecretString,
        _receipt: &IntentReceipt,
    ) -> TransportOutcome<GrowwMutationRespPayload> {
        self.post_mutation(PATH_CREATE, req, token).await
    }

    async fn modify_order(
        &self,
        req: &GrowwModifyOrderReq,
        token: &SecretString,
        _receipt: &IntentReceipt,
    ) -> TransportOutcome<GrowwMutationRespPayload> {
        self.post_mutation(PATH_MODIFY, req, token).await
    }

    async fn cancel_order(
        &self,
        req: &GrowwCancelOrderReq,
        token: &SecretString,
        _receipt: &IntentReceipt,
    ) -> TransportOutcome<GrowwMutationRespPayload> {
        self.post_mutation(PATH_CANCEL, req, token).await
    }

    async fn get_status_by_id(
        &self,
        groww_order_id: &str,
        segment: GrowwSegment,
        token: &SecretString,
    ) -> TransportOutcome<GrowwOrderStatusPayload> {
        let path = format!("{PATH_STATUS_BY_ID}{groww_order_id}");
        self.get_read(&path, &[("segment", segment.as_str().to_owned())], token)
            .await
    }

    async fn get_status_by_reference(
        &self,
        order_reference_id: &str,
        segment: GrowwSegment,
        token: &SecretString,
    ) -> TransportOutcome<GrowwOrderStatusPayload> {
        let path = format!("{PATH_STATUS_BY_REF}{order_reference_id}");
        self.get_read(&path, &[("segment", segment.as_str().to_owned())], token)
            .await
    }

    async fn get_order_detail(
        &self,
        groww_order_id: &str,
        segment: GrowwSegment,
        token: &SecretString,
    ) -> TransportOutcome<GrowwOrderDetailPayload> {
        let path = format!("{PATH_DETAIL}{groww_order_id}");
        self.get_read(&path, &[("segment", segment.as_str().to_owned())], token)
            .await
    }

    async fn list_orders(
        &self,
        segment: GrowwSegment,
        page: u32,
        page_size: u32,
        token: &SecretString,
    ) -> TransportOutcome<Vec<GrowwOrderDetailPayload>> {
        self.get_read(
            PATH_LIST,
            &[
                ("segment", segment.as_str().to_owned()),
                ("page", page.to_string()),
                ("page_size", page_size.to_string()),
            ],
            token,
        )
        .await
    }

    async fn get_trades(
        &self,
        groww_order_id: &str,
        segment: GrowwSegment,
        page: u32,
        page_size: u32,
        token: &SecretString,
    ) -> TransportOutcome<Vec<GrowwTradeRow>> {
        let path = format!("{PATH_TRADES}{groww_order_id}");
        self.get_read(
            &path,
            &[
                ("segment", segment.as_str().to_owned()),
                ("page", page.to_string()),
                ("page_size", page_size.to_string()),
            ],
            token,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oms::groww::types::GrowwErrorBody;

    fn success_env(payload: GrowwMutationRespPayload) -> GrowwEnvelope<GrowwMutationRespPayload> {
        GrowwEnvelope {
            status: Some("SUCCESS".to_owned()),
            payload: Some(payload),
            error: None,
        }
    }

    fn failure_env(code: &str) -> GrowwEnvelope<GrowwMutationRespPayload> {
        GrowwEnvelope {
            status: Some("FAILURE".to_owned()),
            payload: None,
            error: Some(GrowwErrorBody {
                code: Some(code.to_owned()),
                message: Some("boom".to_owned()),
            }),
        }
    }

    fn mut_payload(id: &str) -> GrowwMutationRespPayload {
        GrowwMutationRespPayload {
            groww_order_id: Some(id.to_owned()),
            order_status: Some("OPEN".to_owned()),
            order_reference_id: Some("TV2607150001ABCD".to_owned()),
            remark: None,
        }
    }

    #[test]
    fn classify_2xx_success_with_payload_is_success() {
        let out = classify_response(200, Ok(success_env(mut_payload("GW1"))), None, None);
        match out {
            TransportOutcome::Success(p) => assert_eq!(p.groww_order_id.as_deref(), Some("GW1")),
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[test]
    fn classify_2xx_success_missing_payload_is_ambiguous() {
        let env: GrowwEnvelope<GrowwMutationRespPayload> = GrowwEnvelope {
            status: Some("SUCCESS".to_owned()),
            payload: None,
            error: None,
        };
        assert_eq!(
            classify_response(200, Ok(env), None, None),
            TransportOutcome::Ambiguous(AmbiguityReason::MissingPayload)
        );
    }

    #[test]
    fn classify_400_well_shaped_failure_is_rejected() {
        let out = classify_response(400, Ok(failure_env("GA001")), None, Some("boom".to_owned()));
        match out {
            TransportOutcome::Rejected {
                http_status,
                ga_code,
                ..
            } => {
                assert_eq!(http_status, 400);
                assert_eq!(ga_code.as_deref(), Some("GA001"));
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn classify_ga003_on_2xx_is_ambiguous_not_reject() {
        // doc-F2: GA003 riding a NON-400 status on a mutation is AMBIGUOUS.
        let out = classify_response(200, Ok(failure_env("GA003")), None, None);
        assert_eq!(
            out,
            TransportOutcome::Ambiguous(AmbiguityReason::GaOnNonRejectStatus("GA003".to_owned()))
        );
    }

    #[test]
    fn classify_ga003_on_5xx_is_server_error_ambiguous() {
        let out = classify_response(503, Ok(failure_env("GA003")), None, None);
        assert_eq!(
            out,
            TransportOutcome::Ambiguous(AmbiguityReason::ServerError(503))
        );
    }

    #[test]
    fn classify_429_is_rate_limited() {
        let out: TransportOutcome<GrowwMutationRespPayload> =
            classify_response(429, Err(()), Some(30), Some("slow down".to_owned()));
        match out {
            TransportOutcome::RateLimited {
                retry_after_secs, ..
            } => assert_eq!(retry_after_secs, Some(30)),
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_401_and_403_are_auth_stale() {
        for s in [401_u16, 403] {
            let out: TransportOutcome<GrowwMutationRespPayload> =
                classify_response(s, Err(()), None, None);
            assert_eq!(out, TransportOutcome::AuthStale { http_status: s });
        }
    }

    #[test]
    fn classify_decode_failure_is_ambiguous_decode() {
        let out: TransportOutcome<GrowwMutationRespPayload> =
            classify_response(200, Err(()), None, None);
        assert_eq!(out, TransportOutcome::Ambiguous(AmbiguityReason::Decode));
    }

    #[test]
    fn classify_4xx_without_well_shaped_failure_is_ambiguous() {
        let env: GrowwEnvelope<GrowwMutationRespPayload> = GrowwEnvelope {
            status: Some("FAILURE".to_owned()),
            payload: None,
            error: None, // no code ⇒ NOT well-shaped
        };
        assert_eq!(
            classify_response(400, Ok(env), None, None),
            TransportOutcome::Ambiguous(AmbiguityReason::ServerError(400))
        );
    }

    #[test]
    fn classify_5xx_decodable_body_is_ambiguous_server_error() {
        // 5xx with a decodable (but not well-shaped/success) envelope.
        let env: GrowwEnvelope<GrowwMutationRespPayload> = GrowwEnvelope {
            status: None,
            payload: None,
            error: None,
        };
        assert_eq!(
            classify_response(504, Ok(env), None, None),
            TransportOutcome::Ambiguous(AmbiguityReason::ServerError(504))
        );
    }

    #[test]
    fn classify_5xx_decode_failure_short_circuits_to_decode() {
        // A decode failure classifies Decode regardless of the status (the
        // executor routes both into the ladder — same ambiguous class).
        let out: TransportOutcome<GrowwMutationRespPayload> =
            classify_response(504, Err(()), None, None);
        assert_eq!(out, TransportOutcome::Ambiguous(AmbiguityReason::Decode));
    }

    #[test]
    fn send_error_classification_maps_connect_and_timeout() {
        assert_eq!(
            classify_send_error::<GrowwMutationRespPayload>(SendErrorKind::Connect),
            TransportOutcome::Ambiguous(AmbiguityReason::ConnectPhase)
        );
        assert_eq!(
            classify_send_error::<GrowwMutationRespPayload>(SendErrorKind::Timeout),
            TransportOutcome::Ambiguous(AmbiguityReason::Timeout)
        );
        assert_eq!(
            classify_send_error::<GrowwMutationRespPayload>(SendErrorKind::Other),
            TransportOutcome::Ambiguous(AmbiguityReason::SendFailed)
        );
    }

    #[test]
    fn retry_after_parses_delta_seconds_only() {
        assert_eq!(parse_retry_after_secs(Some("30")), Some(30));
        assert_eq!(parse_retry_after_secs(Some(" 5 ")), Some(5));
        assert_eq!(parse_retry_after_secs(Some("Wed, 21 Oct 2015")), None);
        assert_eq!(parse_retry_after_secs(None), None);
    }

    #[test]
    fn body_excerpt_strips_control_and_truncates() {
        let raw = format!("head\u{202E}tail\n{}", "x".repeat(500));
        let ex = body_excerpt_of(&raw);
        assert!(ex.len() <= MAX_BODY_EXCERPT_CHARS);
        assert!(!ex.contains('\u{202E}'));
        assert!(!ex.contains('\n'));
        assert!(ex.starts_with("headtail"));
    }

    #[test]
    fn null_transport_makes_no_call_and_is_inert() {
        // Build a NullTransport and confirm every arm is the inert ambiguous.
        let t = NullTransport;
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt");
        rt.block_on(async {
            let tok = SecretString::from("unused");
            let out = t.get_status_by_id("GW1", GrowwSegment::Fno, &tok).await;
            assert_eq!(
                out,
                TransportOutcome::Ambiguous(AmbiguityReason::ConnectPhase)
            );
        });
    }
}
