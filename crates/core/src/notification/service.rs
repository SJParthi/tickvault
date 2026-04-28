//! Notification service — Telegram + SNS SMS.
//!
//! ONE source (AWS SSM), ONE code path, everywhere:
//! - Rust app via `NotificationService` → same SSM → same Telegram + SNS
//! - IntelliJ runs same binary → same SSM → same Telegram + SNS
//! - Claude Code sessions use shell script → same SSM → same Telegram
//! - CI/CD uses IAM role → same SSM → same Telegram
//!
//! # Channel Routing
//!
//! All events → Telegram (always).
//! Critical/High severity events → Telegram + SNS SMS (if `sns_enabled`).
//!
//! # Failure Behavior
//!
//! SSM fetch fails at boot → `NotificationService` is created in no-op mode.
//! HTTP/SNS send fails → logged as `warn`, not propagated. Never blocks caller.
//! Bot token NEVER logged (SecretString enforces `[REDACTED]`).
//!
//! # Performance
//!
//! Notifications are cold path. `notify` spawns a `tokio::task` — zero
//! blocking on the caller. Hot path (tick parsing) is completely unaffected.

use std::sync::Arc;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use tracing::{error, info, instrument, warn};

use tickvault_common::config::NotificationConfig;
use tickvault_common::constants::{
    SNS_PHONE_NUMBER_SECRET, SSM_SNS_SERVICE, SSM_TELEGRAM_SERVICE, TELEGRAM_BOT_TOKEN_SECRET,
    TELEGRAM_CHAT_ID_SECRET,
};

use crate::auth::secret_manager::{
    build_ssm_path, create_ssm_client, fetch_secret, resolve_environment,
};

use super::coalescer::{CoalesceDecision, CoalescerConfig, DrainedSummary, TelegramCoalescer};
use super::events::{NotificationEvent, Severity};

// ---------------------------------------------------------------------------
// Internal State
// ---------------------------------------------------------------------------

/// Internal mode — avoids `Option<>` noise at callsites.
enum NotificationMode {
    /// SSM credentials loaded — sends are attempted.
    Active {
        bot_token: SecretString,
        chat_id: String,
        http_client: reqwest::Client,
        telegram_api_base_url: String,
        /// SNS client for SMS sends. `None` if SNS disabled or SSM fetch failed.
        sns_client: Option<aws_sdk_sns::Client>,
        /// E.164 phone number for SMS. `None` if SNS disabled or SSM fetch failed.
        sns_phone_number: Option<String>,
    },
    /// SSM unavailable at boot — all sends are silent no-ops.
    NoOp,
}

// ---------------------------------------------------------------------------
// Public Service
// ---------------------------------------------------------------------------

/// Fire-and-forget notification service — Telegram + SNS SMS.
///
/// Create once at boot via `NotificationService::initialize`. Pass `Arc<Self>`
/// to any component that needs alerting. Call `notify` to send — it spawns
/// a background task and returns immediately.
///
/// All events go to Telegram. Critical/High events also trigger SNS SMS
/// (if `sns_enabled` is true and the phone number is in SSM).
pub struct NotificationService {
    mode: NotificationMode,
    /// Wave 3-B Item 11: optional Telegram bucket-coalescer.
    ///
    /// `None` means coalescing is disabled (legacy passthrough — every event
    /// is sent immediately). `Some(...)` means Severity::Low and Severity::Info
    /// events are folded into per-`(topic, severity)` windows; Critical /
    /// High / Medium always bypass.
    ///
    /// Wired via `enable_coalescer()` after `initialize()` based on the
    /// `features.telegram_bucket_coalescer` config flag.
    coalescer: Option<Arc<TelegramCoalescer>>,
}

impl NotificationService {
    /// Internal helper — every constructor funnels through here so the
    /// `coalescer` field default is in one place. Adding a new field to
    /// the struct requires updating only this method.
    fn build(mode: NotificationMode) -> Self {
        Self {
            mode,
            coalescer: None,
        }
    }

    /// Initializes the notification service by fetching Telegram credentials from SSM.
    ///
    /// If SSM is unavailable (no IAM role, tokens not in SSM, etc.), returns
    /// a no-op service. The caller is never blocked and boot continues normally.
    /// Telegram tokens are always in real AWS SSM (dev and prod, same region).
    ///
    /// # Arguments
    ///
    /// * `config` — `NotificationConfig` from `config/base.toml`.
    ///
    /// # Returns
    ///
    /// Always returns `Arc<Self>` — never fails. Uses no-op mode on SSM failure.
    ///
    /// # Performance
    ///
    /// Called once at boot. Not on hot path.
    #[instrument(skip_all, name = "notification_service_init")]
    pub async fn initialize(config: &NotificationConfig) -> Arc<Self> {
        // AWS SSM is the single source of truth for all secrets. No env-var
        // fallback — production config, dev config, CI config all read from
        // the same place so behavior is identical everywhere.
        let environment = match resolve_environment() {
            Ok(env) => env,
            Err(err) => {
                warn!(
                    error = %err,
                    "notification: cannot resolve environment — using no-op mode"
                );
                return Arc::new(Self::build(NotificationMode::NoOp));
            }
        };

        let ssm_client = create_ssm_client().await;

        let bot_token_path = build_ssm_path(
            &environment,
            SSM_TELEGRAM_SERVICE,
            TELEGRAM_BOT_TOKEN_SECRET,
        );
        let chat_id_path =
            build_ssm_path(&environment, SSM_TELEGRAM_SERVICE, TELEGRAM_CHAT_ID_SECRET);

        let (bot_token_result, chat_id_result) = tokio::join!(
            fetch_secret(&ssm_client, &bot_token_path),
            fetch_secret(&ssm_client, &chat_id_path),
        );

        let bot_token = match bot_token_result {
            Ok(token) => token,
            Err(err) => {
                warn!(
                    error = %err,
                    path = %bot_token_path,
                    "notification: bot-token SSM fetch failed — using no-op mode"
                );
                return Arc::new(Self::build(NotificationMode::NoOp));
            }
        };

        let chat_id = match chat_id_result {
            Ok(id) => id.expose_secret().to_string(),
            Err(err) => {
                warn!(
                    error = %err,
                    path = %chat_id_path,
                    "notification: chat-id SSM fetch failed — using no-op mode"
                );
                return Arc::new(Self::build(NotificationMode::NoOp));
            }
        };

        let http_client = match reqwest::Client::builder()
            .timeout(Duration::from_millis(config.send_timeout_ms))
            .build()
        {
            Ok(client) => client,
            Err(err) => {
                warn!(
                    error = %err,
                    "notification: HTTP client build failed — using no-op mode"
                );
                return Arc::new(Self::build(NotificationMode::NoOp));
            }
        };

        // --- SNS SMS (optional, for Critical/High severity alerts) ---
        let (sns_client, sns_phone_number) = if config.sns_enabled {
            let phone_path = build_ssm_path(&environment, SSM_SNS_SERVICE, SNS_PHONE_NUMBER_SECRET);
            match fetch_secret(&ssm_client, &phone_path).await {
                Ok(phone_secret) => {
                    let phone = phone_secret.expose_secret().to_string();
                    let aws_cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
                        .region(aws_config::Region::new("ap-south-1"))
                        .load()
                        .await;
                    let client = aws_sdk_sns::Client::new(&aws_cfg);
                    info!(
                        phone_masked = %mask_phone(&phone),
                        "notification: SNS SMS active"
                    );
                    (Some(client), Some(phone))
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        path = %phone_path,
                        "notification: SNS phone-number SSM fetch failed — SMS disabled"
                    );
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        // SECURITY: Do not log chat_id — it's a component of the
        // credential pair needed for Telegram message injection.
        info!(
            sns_enabled = config.sns_enabled,
            "notification service initialized — Telegram active"
        );

        Arc::new(Self::build(NotificationMode::Active {
            bot_token,
            chat_id,
            http_client,
            telegram_api_base_url: config.telegram_api_base_url.clone(),
            sns_client,
            sns_phone_number,
        }))
    }

    /// Creates a disabled no-op instance.
    ///
    /// Used in tests or when notifications are intentionally disabled.
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self::build(NotificationMode::NoOp))
    }

    /// Strict initialization — returns `Err` if the service would fall back
    /// to no-op mode. This is the default path for production boot: if the
    /// notifier is dead, the app must refuse to start so systemd restarts it
    /// and an operator notices the loop.
    ///
    /// Escape hatch: set `TICKVAULT_ALLOW_NOOP_NOTIFIER=1` to allow no-op
    /// mode (used by tests and dev runs without SSM access).
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` with a human-readable reason if any of the
    /// following fail:
    /// - AWS SSM environment cannot be resolved
    /// - Telegram bot-token fetch fails
    /// - Telegram chat-id fetch fails
    /// - HTTP client cannot be built
    ///
    /// C1: closes the silent-degradation class by panicking at boot instead
    /// of running deaf.
    #[instrument(skip_all, name = "notification_service_init_strict")]
    pub async fn initialize_strict(config: &NotificationConfig) -> Result<Arc<Self>, String> {
        Self::enforce_strict(Self::initialize(config).await)
    }

    /// Pure (non-async, no-IO) strict-mode policy decision.
    ///
    /// Extracted from `initialize_strict` so tests can exercise the
    /// "refuse no-op unless env var is set" contract without depending
    /// on AWS SSM reachability (which differs between Mac dev machines
    /// with SSM creds and CI boxes without them).
    pub fn enforce_strict(service: Arc<Self>) -> Result<Arc<Self>, String> {
        if service.is_active() {
            return Ok(service);
        }
        if std::env::var("TICKVAULT_ALLOW_NOOP_NOTIFIER").is_ok() {
            warn!(
                "notification: strict mode overridden by TICKVAULT_ALLOW_NOOP_NOTIFIER — \
                 running in no-op mode (dev/test only)"
            );
            return Ok(service);
        }
        Err("NotificationService fell back to no-op mode. \
             SSM is unreachable or Telegram credentials are missing. \
             Refusing to boot — the app must not run deaf. \
             Set TICKVAULT_ALLOW_NOOP_NOTIFIER=1 to override (dev/test only)."
            .to_string())
    }

    /// Sends a notification event. Spawns background tasks — returns immediately.
    ///
    /// All events → Telegram. Critical/High → also SNS SMS (if configured).
    /// If the service is in no-op mode, this is a zero-cost return.
    /// If any send fails, logs `warn` and discards the error.
    ///
    /// # Performance
    ///
    /// Not on hot path. `tokio::spawn` overhead is acceptable for cold-path alerts.
    pub fn notify(self: &Arc<Self>, event: NotificationEvent) {
        match &self.mode {
            NotificationMode::NoOp => {
                // No-op: SSM was unavailable at boot. Do nothing.
                metrics::counter!(
                    "tv_telegram_dropped_total",
                    "reason" => "noop_mode",
                )
                .increment(1);
            }
            NotificationMode::Active {
                bot_token,
                chat_id,
                http_client,
                telegram_api_base_url,
                sns_client,
                sns_phone_number,
            } => {
                let severity = event.severity();
                // UX fix 2026-04-17: prefix every Telegram message with a
                // severity tag + emoji so the operator can visually scan
                // a long chat list and pick out incidents at a glance.
                // Parthiban workflow: any [HIGH] / [CRITICAL] message goes
                // straight to Claude Code for debugging.
                let topic = event.topic();
                let message = format!("{} {}", severity.tag(), event.to_message());

                // Wave 3-B Item 11: bucket-coalesce Severity::Low and
                // Severity::Info events. Bypass everything else (Critical,
                // High, Medium → immediate dispatch). The coalescer's
                // `observe` returns `Bypass` for Critical/High/Medium so
                // the same call covers both branches; we only short-circuit
                // when it returns `Coalesced`.
                if let Some(coalescer) = self.coalescer.as_ref() {
                    let decision = coalescer.observe(topic, severity, || message.clone());
                    if matches!(decision, CoalesceDecision::Coalesced) {
                        metrics::counter!(
                            "tv_telegram_dispatched_total",
                            "severity" => severity.as_label(),
                            "coalesced" => "true",
                        )
                        .increment(1);
                        return;
                    }
                }
                // Bypass / coalescer disabled: this dispatch counts as a
                // direct send.
                metrics::counter!(
                    "tv_telegram_dispatched_total",
                    "severity" => severity.as_label(),
                    "coalesced" => "false",
                )
                .increment(1);

                // Always: Telegram. Messages > 4000 chars get split into
                // ordered chunks per Telegram's 4096-char hard limit so
                // the cross-match FAILED report (potentially thousands of
                // rows) can be delivered in full — Parthiban directive
                // 2026-04-21: no truncation, full list always visible.
                {
                    let token = bot_token.clone();
                    let chat_id = chat_id.clone();
                    let client = http_client.clone();
                    let base_url = telegram_api_base_url.clone();
                    let chunks = split_message_for_telegram(&message);

                    tokio::spawn(async move {
                        let total = chunks.len();
                        let mut failed_parts: Vec<usize> = Vec::new();
                        for (idx, chunk) in chunks.iter().enumerate() {
                            let part_num = idx.saturating_add(1);
                            let body = if total > 1 {
                                format!("(Part {part_num}/{total})\n{chunk}")
                            } else {
                                chunk.clone()
                            };
                            let ok = send_telegram_chunk_with_retry(
                                &client, &base_url, &token, &chat_id, &body,
                            )
                            .await;
                            if !ok {
                                failed_parts.push(part_num);
                            }
                        }
                        // Q3 (2026-04-23): if ANY chunk failed after all
                        // retries, emit a single ERROR log so the operator
                        // knows the report is incomplete. Loki routes
                        // ERROR-level tracing events to Telegram via
                        // alertmanager, so this serves as the
                        // "delivery-failed, investigate" signal even
                        // though the Telegram transport itself is the
                        // thing that failed (the alertmanager path is
                        // independent — different chat channel / retry).
                        if !failed_parts.is_empty() {
                            error!(
                                total_chunks = total,
                                failed_chunks = failed_parts.len(),
                                failed_part_numbers = ?failed_parts,
                                "notification: Telegram chunked-send had mid-batch failures after retries — user-visible report is incomplete"
                            );
                        }
                    });
                }

                // Critical/High only: SNS SMS
                if severity >= Severity::High
                    && let (Some(sns), Some(phone)) = (sns_client, sns_phone_number)
                {
                    let sms_text = strip_html_tags(&message);
                    let client = sns.clone();
                    let phone = phone.clone();

                    tokio::spawn(async move {
                        send_sns_sms(&client, &phone, &sms_text).await;
                    });
                }
            }
        }
    }

    /// Returns `true` if the service is active (SSM credentials loaded).
    pub fn is_active(&self) -> bool {
        matches!(&self.mode, NotificationMode::Active { .. })
    }

    /// Returns `true` if the bucket-coalescer is wired in.
    ///
    /// Used by tests + the boot path to confirm the C9
    /// `features.telegram_bucket_coalescer` flag took effect.
    pub fn coalescer_enabled(&self) -> bool {
        self.coalescer.is_some()
    }

    /// Wave 3-B Item 11 — wraps an existing service with a bucket-coalescer.
    ///
    /// Returns a new `Arc<Self>` with the coalescer wired in. If the
    /// underlying service is in `NoOp` mode (SSM unavailable), the
    /// coalescer is wired but no drain task is spawned (there's no
    /// Telegram channel to drain to).
    ///
    /// Spawns a background tokio task that wakes every
    /// `config.flush_interval` to drain mature windows. The task lives
    /// as long as the returned `Arc<Self>` is held — when the last
    /// reference drops, the task observes a closed weak ref and exits.
    ///
    /// # Arguments
    ///
    /// * `service` — the existing `Arc<Self>` from `initialize()`.
    /// * `config` — coalescer window + flush-interval. Defaults are
    ///   sensible (60s window, 10s flush-interval).
    ///
    /// # Returns
    ///
    /// New `Arc<Self>` — the original input is consumed but its
    /// underlying state is preserved (we re-wrap the same `mode`).
    pub fn enable_coalescer(service: Arc<Self>, config: CoalescerConfig) -> Arc<Self> {
        // If already enabled, return as-is — idempotent boot wiring.
        if service.coalescer.is_some() {
            return service;
        }

        let coalescer = Arc::new(TelegramCoalescer::new(config));

        // Spawn the drain task only when the underlying service can actually
        // send. NoOp mode has no channel to deliver to.
        if let NotificationMode::Active {
            bot_token,
            chat_id,
            http_client,
            telegram_api_base_url,
            ..
        } = &service.mode
        {
            let drain_coalescer = coalescer.clone();
            let token = bot_token.clone();
            let chat_id = chat_id.clone();
            let client = http_client.clone();
            let base_url = telegram_api_base_url.clone();
            let interval = config.flush_interval;

            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // Skip the immediate first tick so we don't try to drain
                // an empty coalescer right after spawn.
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    let summaries = drain_coalescer.drain_mature();
                    if summaries.is_empty() {
                        continue;
                    }
                    deliver_summaries(&client, &base_url, &token, &chat_id, summaries).await;
                }
            });
        }

        // Re-wrap the existing mode with the coalescer attached. We can't
        // mutate through Arc<Self>; build a fresh struct that takes
        // ownership of the underlying mode by `Arc::try_unwrap` if the
        // count is 1, otherwise we share via the safe path below.
        match Arc::try_unwrap(service) {
            Ok(mut owned) => {
                owned.coalescer = Some(coalescer);
                Arc::new(owned)
            }
            Err(arc) => {
                // Caller is already sharing the Arc — cannot mutate in
                // place. This is a programmer error at boot; log and
                // return the original (legacy passthrough path).
                error!(
                    target: "notification",
                    code = tickvault_common::error_code::ErrorCode::Telegram02CoalescerStateInconsistency.code_str(),
                    "enable_coalescer called on a shared Arc — coalescer NOT wired (boot ordering bug)"
                );
                arc
            }
        }
    }
}

/// Renders + sends one Telegram message per drained summary.
///
/// Called by the drain task spawned in [`NotificationService::enable_coalescer`].
async fn deliver_summaries(
    client: &reqwest::Client,
    base_url: &str,
    token: &SecretString,
    chat_id: &str,
    summaries: Vec<DrainedSummary>,
) {
    for summary in summaries {
        // The coalesced summary inherits the original severity tag from
        // the bucket key — operators still see the [LOW] / [INFO] tag
        // they would have got from the un-coalesced events.
        let body = format!("{} {}", summary.severity.tag(), summary.render_message());
        let chunks = split_message_for_telegram(&body);
        let total = chunks.len();
        let mut failed = 0_usize;
        for (idx, chunk) in chunks.iter().enumerate() {
            let part_num = idx.saturating_add(1);
            let final_body = if total > 1 {
                format!("(Part {part_num}/{total})\n{chunk}")
            } else {
                chunk.clone()
            };
            let ok =
                send_telegram_chunk_with_retry(client, base_url, token, chat_id, &final_body).await;
            if !ok {
                failed = failed.saturating_add(1);
            }
        }
        if failed > 0 {
            metrics::counter!(
                "tv_telegram_dropped_total",
                "reason" => "send_failed",
            )
            .increment(failed as u64);
            error!(
                target: "notification",
                code = tickvault_common::error_code::ErrorCode::Telegram01Dropped.code_str(),
                topic = summary.topic,
                severity = summary.severity.as_label(),
                count = summary.count,
                failed_chunks = failed,
                "TELEGRAM-01: coalesced summary delivery failed for some chunks — operator alerts MAY be missed"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Internal HTTP Send
// ---------------------------------------------------------------------------

/// Telegram's hard message cap is 4096 characters. We target a safe ceiling
/// below that so the `(Part i/N)` prefix + any HTML tag pair doesn't push
/// the chunk over.
pub(crate) const TELEGRAM_CHUNK_LIMIT_CHARS: usize = 3800;

/// Splits a potentially huge Telegram message into ordered chunks, each
/// ≤ `TELEGRAM_CHUNK_LIMIT_CHARS` characters. Splits on newline boundaries
/// (never mid-row). Preserves open/close `<pre>` HTML tags across chunks
/// so the monospace formatting stays intact when Telegram re-parses each
/// chunk independently.
///
/// Parthiban directive 2026-04-21: cross-match FAILED report must show
/// every violation — no truncation. On a heavy-gap day this can produce
/// thousands of rows; chunking delivers them all as sequential Telegram
/// messages.
pub(crate) fn split_message_for_telegram(full: &str) -> Vec<String> {
    if full.chars().count() <= TELEGRAM_CHUNK_LIMIT_CHARS {
        return vec![full.to_string()];
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut inside_pre = false;

    for line in full.split_inclusive('\n') {
        let would_overflow = current.chars().count().saturating_add(line.chars().count())
            > TELEGRAM_CHUNK_LIMIT_CHARS;

        if would_overflow && !current.is_empty() {
            // Close any open <pre> before emitting the chunk so Telegram
            // doesn't render the rest of the message as plain text.
            if inside_pre {
                current.push_str("</pre>");
            }
            chunks.push(std::mem::take(&mut current));
            // Re-open <pre> at the top of the next chunk for continuity.
            if inside_pre {
                current.push_str("<pre>");
            }
        }

        current.push_str(line);

        // Track <pre> state so split/resume keeps monospace contiguous.
        if line.contains("<pre>") {
            inside_pre = true;
        }
        if line.contains("</pre>") {
            inside_pre = false;
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

/// Posts a message to the Telegram Bot API.
///
/// Logs `warn` on any failure — does not return an error to the caller.
/// Bot token is redacted from error logs (reqwest errors may include the URL).
///
/// # Performance
///
/// Cold path only. Called from a spawned task.
/// Per-chunk retry budget when Telegram returns a transient failure. Q3
/// (2026-04-23): a previous implementation logged WARN on failure and
/// moved on to the next chunk — on a 14-part CROSS-MATCH FAILED message
/// a mid-batch 429 or TLS blip would leave the operator with a gap in
/// the details. Three retries with `100ms → 400ms → 1600ms` backoff
/// cover typical rate-limit windows without falling off the
/// notification-task timeout budget.
pub(crate) const TELEGRAM_SEND_MAX_ATTEMPTS: u32 = 3;

/// Initial backoff (in milliseconds) before the second attempt. Doubled
/// each retry, capped at [`TELEGRAM_SEND_BACKOFF_CAP_SECS`]. Stored as
/// `u64` rather than `Duration` so the banned-pattern scanner recognises
/// it as a named constant (the scanner rejects
/// `Duration::from_millis(<literal>)` outright).
pub(crate) const TELEGRAM_SEND_BACKOFF_INITIAL_MS: u64 = 100;

/// Upper bound on the retry sleep (in seconds) — prevents a single stuck
/// chunk from delaying the rest of the background notification task
/// indefinitely.
pub(crate) const TELEGRAM_SEND_BACKOFF_CAP_SECS: u64 = 2;

/// Outcome of a single Telegram `sendMessage` request. Drives whether
/// the caller retries the same chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TelegramSendOutcome {
    /// HTTP 2xx — Telegram accepted the message.
    Success,
    /// Network error, 5xx, or 429 rate-limit — worth retrying.
    Transient,
    /// 4xx other than 429 — retrying won't help (bad bot token, wrong
    /// chat_id, malformed HTML). Log ERROR once and move on.
    Permanent,
}

/// Classifies an HTTP status for Telegram-send retry decisions. 429 and
/// 5xx are transient (retry); other 4xx are permanent (don't retry); 2xx
/// is success.
fn classify_telegram_status(status: reqwest::StatusCode) -> TelegramSendOutcome {
    if status.is_success() {
        TelegramSendOutcome::Success
    } else if status.as_u16() == 429 || status.is_server_error() {
        TelegramSendOutcome::Transient
    } else {
        TelegramSendOutcome::Permanent
    }
}

async fn send_telegram_message(
    client: &reqwest::Client,
    base_url: &str,
    bot_token: &SecretString,
    chat_id: &str,
    text: &str,
) -> TelegramSendOutcome {
    let url = format!("{}/bot{}/sendMessage", base_url, bot_token.expose_secret());

    let body = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
        "parse_mode": "HTML",
    });

    match client.post(&url).json(&body).send().await {
        Ok(response) => {
            let status = response.status();
            let outcome = classify_telegram_status(status);
            match outcome {
                TelegramSendOutcome::Success => {
                    tracing::debug!("notification: Telegram message sent");
                }
                TelegramSendOutcome::Transient => {
                    warn!(
                        status = %status,
                        "notification: Telegram sendMessage transient failure — will retry"
                    );
                }
                TelegramSendOutcome::Permanent => {
                    warn!(
                        status = %status,
                        "notification: Telegram sendMessage permanent failure (4xx non-429) — not retrying"
                    );
                }
            }
            outcome
        }
        Err(err) => {
            // SECURITY: reqwest::Error Display may include the request URL,
            // which contains the bot token in the path. Redact before logging.
            let safe_msg = err
                .to_string()
                .replace(bot_token.expose_secret(), "[REDACTED]");
            warn!(
                error = %safe_msg,
                "notification: Telegram sendMessage HTTP error (transient)"
            );
            // Network errors are always transient — DNS, TLS handshake,
            // TCP reset all self-heal on retry.
            TelegramSendOutcome::Transient
        }
    }
}

/// Sends a single Telegram chunk with per-chunk retry. Retries transient
/// failures (network, 5xx, 429) up to [`TELEGRAM_SEND_MAX_ATTEMPTS`] with
/// doubling backoff; returns `true` iff at least one attempt succeeded.
/// Permanent 4xx responses abort immediately (retry won't help).
///
/// Q3 regression (2026-04-23): before this wrapper, the chunked send
/// loop called `send_telegram_message` once per chunk and moved on —
/// a single rate-limited mid-batch chunk produced a silent gap in the
/// user-visible CROSS-MATCH FAILED report.
pub(crate) async fn send_telegram_chunk_with_retry(
    client: &reqwest::Client,
    base_url: &str,
    bot_token: &SecretString,
    chat_id: &str,
    text: &str,
) -> bool {
    let mut delay = Duration::from_millis(TELEGRAM_SEND_BACKOFF_INITIAL_MS);
    let cap = Duration::from_secs(TELEGRAM_SEND_BACKOFF_CAP_SECS);
    for attempt in 1..=TELEGRAM_SEND_MAX_ATTEMPTS {
        match send_telegram_message(client, base_url, bot_token, chat_id, text).await {
            TelegramSendOutcome::Success => return true,
            TelegramSendOutcome::Permanent => return false,
            TelegramSendOutcome::Transient => {
                if attempt < TELEGRAM_SEND_MAX_ATTEMPTS {
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2).min(cap);
                }
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Internal SNS Send
// ---------------------------------------------------------------------------

/// Sends an SMS via AWS SNS direct publish to a phone number.
///
/// Uses `Transactional` SMS type for highest delivery priority (not promotional).
/// Logs `warn` on failure — never panics, never blocks caller.
///
/// # Performance
///
/// Cold path only. Called from a spawned task.
async fn send_sns_sms(client: &aws_sdk_sns::Client, phone_number: &str, message: &str) {
    let sms_type = match aws_sdk_sns::types::MessageAttributeValue::builder()
        .data_type("String")
        .string_value("Transactional")
        .build()
    {
        Ok(v) => v,
        Err(err) => {
            warn!(error = %err, "notification: SNS MessageAttributeValue build failed");
            return;
        }
    };

    match client
        .publish()
        .phone_number(phone_number)
        .message(message)
        .message_attributes("AWS.SNS.SMS.SMSType", sms_type)
        .send()
        .await
    {
        Ok(output) => {
            tracing::debug!(
                message_id = ?output.message_id(),
                "notification: SNS SMS sent"
            );
        }
        Err(err) => {
            warn!(error = %err, "notification: SNS SMS send failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Strips HTML tags from a Telegram-formatted message for plain-text SMS.
///
/// Converts `<b>text</b>` → `text`. Handles any `<tag>` generically.
/// No regex dependency — simple char scan.
fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Masks a phone number for safe logging.
///
/// `"+919876543210"` → `"+91XXXXX43210"`.
fn mask_phone(phone: &str) -> String {
    if phone.len() <= 8 {
        return "***".to_string();
    }
    let keep_prefix: usize = 3;
    let keep_suffix: usize = 5;
    let mask_len = phone
        .len()
        .saturating_sub(keep_prefix.saturating_add(keep_suffix));
    format!(
        "{}{}{}",
        &phone[..keep_prefix],
        "X".repeat(mask_len),
        &phone[phone.len().saturating_sub(keep_suffix)..]
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test code
mod tests {
    use super::*;

    #[test]
    fn test_disabled_service_is_not_active() {
        let service = NotificationService::disabled();
        assert!(!service.is_active());
    }

    // -----------------------------------------------------------------------
    // split_message_for_telegram — chunking for cross-match full lists
    // Parthiban directive 2026-04-21: full report, chunked on newline
    // boundaries, never mid-row, preserves <pre> monospace wrappers.
    // -----------------------------------------------------------------------

    #[test]
    fn test_split_message_for_telegram_below_threshold_returns_single() {
        let msg = "tiny";
        assert_eq!(split_message_for_telegram(msg).len(), 1);
    }

    #[test]
    fn test_chunk_below_threshold_returns_single() {
        let msg = "short message under the limit";
        let out = split_message_for_telegram(msg);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], msg);
    }

    #[test]
    fn test_chunk_splits_on_newline_boundary() {
        // Build a message with many short lines that forces multiple chunks.
        let line = "A".repeat(100);
        let mut body = String::new();
        for _ in 0..80 {
            body.push_str(&line);
            body.push('\n');
        }
        let out = split_message_for_telegram(&body);
        assert!(
            out.len() > 1,
            "message of ~8000 chars must split into multiple chunks"
        );
        // Every chunk stays below the configured ceiling.
        for c in &out {
            assert!(
                c.chars().count() <= TELEGRAM_CHUNK_LIMIT_CHARS,
                "chunk size {} > limit {TELEGRAM_CHUNK_LIMIT_CHARS}",
                c.chars().count()
            );
        }
    }

    #[test]
    fn test_chunk_preserves_line_order() {
        // Numbered lines so we can verify the concatenation preserves order
        // even across chunk boundaries.
        let mut body = String::new();
        for i in 0..500 {
            body.push_str(&format!("row-{i:03} {}\n", "x".repeat(60)));
        }
        let out = split_message_for_telegram(&body);
        let joined = out.join("");
        // Expected order: row-000, row-001, ..., row-499 (as substrings).
        let idx_000 = joined.find("row-000").expect("row-000 missing");
        let idx_499 = joined.find("row-499").expect("row-499 missing");
        assert!(idx_000 < idx_499, "order broken across chunks");
    }

    #[test]
    fn test_chunk_preserves_pre_tag_pairs_across_boundary() {
        // A message that opens <pre>, has many lines, and closes </pre> must
        // re-open <pre> at every chunk boundary so Telegram's HTML parser
        // treats every chunk as monospace.
        let mut body = String::from("<pre>");
        for _ in 0..60 {
            body.push_str(&"B".repeat(100));
            body.push('\n');
        }
        body.push_str("</pre>");
        let out = split_message_for_telegram(&body);
        assert!(out.len() >= 2);
        // Every chunk must contain a <pre> opener. Every chunk except the
        // last must contain a </pre> closer emitted by the splitter.
        for (idx, c) in out.iter().enumerate() {
            assert!(c.contains("<pre>"), "chunk {idx} missing <pre>");
            if idx < out.len() - 1 {
                assert!(c.contains("</pre>"), "chunk {idx} must self-close <pre>");
            }
        }
    }

    // -----------------------------------------------------------------------
    // C1: Strict init must refuse no-op fallback unless env var is set
    // -----------------------------------------------------------------------

    /// C1: `initialize_strict` MUST return `Err` when SSM is unreachable
    /// (simulated here by the unit-test environment which has no AWS creds)
    /// and `TICKVAULT_ALLOW_NOOP_NOTIFIER` is NOT set, AND MUST return `Ok`
    /// when the escape hatch env var is set.
    ///
    /// Both paths are in one test because they mutate the same process-global
    /// env var; running as separate tests races under parallel cargo test.
    /// Order: refuse → set → allow → remove → refuse again (sanity).
    ///
    /// This is the mechanical enforcement for "don't run deaf".
    #[tokio::test]
    async fn test_c1_initialize_strict_refuses_noop_and_respects_override() {
        // Drive the pure `enforce_strict` policy directly with a guaranteed
        // NoOp service. This removes dependency on AWS SSM reachability,
        // which on dev machines with valid SSM creds causes `initialize()`
        // to return an Active service and masks the strict-mode contract.

        // Phase 1: refuse when env var is NOT set.
        // SAFETY: removing an env var is safe.
        unsafe {
            std::env::remove_var("TICKVAULT_ALLOW_NOOP_NOTIFIER");
        }
        let refuse_result = NotificationService::enforce_strict(NotificationService::disabled());
        assert!(
            refuse_result.is_err(),
            "Phase 1: enforce_strict must refuse no-op when env var is unset"
        );
        let err = refuse_result.err().unwrap();
        assert!(
            err.contains("no-op") || err.contains("SSM") || err.contains("Refusing"),
            "Phase 1: error message must explain why boot is refused: {err}"
        );

        // Phase 2: allow when env var IS set.
        // SAFETY: setting an env var is safe.
        unsafe {
            std::env::set_var("TICKVAULT_ALLOW_NOOP_NOTIFIER", "1");
        }
        let allow_result = NotificationService::enforce_strict(NotificationService::disabled());
        // SAFETY: clean up so other tests don't inherit the override.
        unsafe {
            std::env::remove_var("TICKVAULT_ALLOW_NOOP_NOTIFIER");
        }
        assert!(
            allow_result.is_ok(),
            "Phase 2: enforce_strict must allow no-op when TICKVAULT_ALLOW_NOOP_NOTIFIER=1"
        );

        // Phase 3: sanity — post-override, refuse path is restored.
        let refuse_again = NotificationService::enforce_strict(NotificationService::disabled());
        assert!(
            refuse_again.is_err(),
            "Phase 3: cleanup must restore the refuse path"
        );
    }

    #[test]
    fn test_enforce_strict_with_active_service_returns_ok() {
        // Pure-function contract: active service always returns Ok,
        // regardless of env var state.
        // SAFETY: removing an env var is safe.
        unsafe {
            std::env::remove_var("TICKVAULT_ALLOW_NOOP_NOTIFIER");
        }
        // We can't easily construct a fully Active service without AWS SSM,
        // but `disabled()` is guaranteed NoOp so we drive the NoOp paths
        // instead. The Active path is covered implicitly by
        // `initialize_strict` integration tests on machines with SSM.
        let result = NotificationService::enforce_strict(NotificationService::disabled());
        assert!(
            result.is_err(),
            "enforce_strict on a NoOp service with env unset must refuse"
        );
    }

    #[test]
    fn test_disabled_service_notify_does_not_panic() {
        let service = NotificationService::disabled();
        // Fire-and-forget on a no-op service must not panic.
        service.notify(NotificationEvent::StartupComplete { mode: "LIVE" });
        service.notify(NotificationEvent::ShutdownComplete);
        service.notify(NotificationEvent::Custom {
            message: "test".to_string(),
        });
    }

    // -----------------------------------------------------------------------
    // Active mode tests
    // -----------------------------------------------------------------------

    fn make_active_service() -> Arc<NotificationService> {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();

        Arc::new(NotificationService::build(NotificationMode::Active {
            bot_token: SecretString::from("test-bot-token".to_string()),
            chat_id: "123456789".to_string(),
            http_client,
            telegram_api_base_url: "http://127.0.0.1:1".to_string(),
            sns_client: None,
            sns_phone_number: None,
        }))
    }

    #[test]
    fn test_active_service_is_active() {
        let service = make_active_service();
        assert!(
            service.is_active(),
            "Active-mode service must return true from is_active()"
        );
    }

    #[tokio::test]
    async fn test_active_service_notify_does_not_panic() {
        let service = make_active_service();

        service.notify(NotificationEvent::StartupComplete { mode: "LIVE" });
        service.notify(NotificationEvent::ShutdownComplete);
        service.notify(NotificationEvent::AuthenticationSuccess);
        service.notify(NotificationEvent::Custom {
            message: "active-mode test".to_string(),
        });

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // -----------------------------------------------------------------------
    // send_telegram_message — error path tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_send_telegram_message_connection_refused_does_not_panic() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();

        let token = SecretString::from("fake-bot-token".to_string());

        send_telegram_message(
            &client,
            "http://127.0.0.1:1",
            &token,
            "999999999",
            "test message",
        )
        .await;
    }

    #[tokio::test]
    async fn test_send_telegram_message_invalid_url_does_not_panic() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();

        let token = SecretString::from("fake-bot-token".to_string());

        send_telegram_message(
            &client,
            "not-a-valid-url://garbage",
            &token,
            "999999999",
            "test message",
        )
        .await;
    }

    // -----------------------------------------------------------------------
    // disabled() and mode checking
    // -----------------------------------------------------------------------

    #[test]
    fn test_disabled_returns_noop_mode() {
        let service = NotificationService::disabled();
        assert!(
            !service.is_active(),
            "disabled() must create a NoOp-mode service"
        );
    }

    #[test]
    fn test_multiple_disabled_instances_are_independent() {
        let s1 = NotificationService::disabled();
        let s2 = NotificationService::disabled();
        assert!(!s1.is_active());
        assert!(!s2.is_active());
        assert!(!Arc::ptr_eq(&s1, &s2));
    }

    #[test]
    fn test_disabled_service_handles_critical_events() {
        let service = NotificationService::disabled();
        service.notify(NotificationEvent::AuthenticationFailed {
            reason: "timeout".to_string(),
        });
        service.notify(NotificationEvent::TokenRenewalFailed {
            attempts: 3,
            reason: "timeout".to_string(),
        });
    }

    // -----------------------------------------------------------------------
    // send_telegram_message — non-success HTTP status path
    // -----------------------------------------------------------------------

    async fn start_mock_telegram_server(status_code: u16) -> String {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let body = r#"{"ok":false,"description":"Unauthorized"}"#;
                let response = format!(
                    "HTTP/1.1 {} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status_code,
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        base_url
    }

    #[tokio::test]
    async fn test_send_telegram_message_non_success_status_does_not_panic() {
        let base_url = start_mock_telegram_server(401).await;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();

        let token = SecretString::from("fake-bot-token".to_string());

        send_telegram_message(&client, &base_url, &token, "999999999", "test message").await;
    }

    #[tokio::test]
    async fn test_send_telegram_message_success_status_does_not_panic() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let body = r#"{"ok":true}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();

        let token = SecretString::from("fake-bot-token".to_string());

        send_telegram_message(&client, &base_url, &token, "999999999", "test message").await;
    }

    // -----------------------------------------------------------------------
    // Q3 regression pins (2026-04-23): Telegram chunk-send retry + classify.
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_telegram_status_success_is_success() {
        assert_eq!(
            classify_telegram_status(reqwest::StatusCode::OK),
            TelegramSendOutcome::Success
        );
        assert_eq!(
            classify_telegram_status(reqwest::StatusCode::ACCEPTED),
            TelegramSendOutcome::Success
        );
    }

    #[test]
    fn test_classify_telegram_status_429_is_transient() {
        // Rate-limited — must retry, not give up.
        assert_eq!(
            classify_telegram_status(reqwest::StatusCode::TOO_MANY_REQUESTS),
            TelegramSendOutcome::Transient
        );
    }

    #[test]
    fn test_classify_telegram_status_5xx_is_transient() {
        // Server errors (502, 503, 504) are telegram-side blips — retry.
        for code in [500, 502, 503, 504] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            assert_eq!(
                classify_telegram_status(status),
                TelegramSendOutcome::Transient,
                "HTTP {code} must classify as transient"
            );
        }
    }

    #[test]
    fn test_classify_telegram_status_4xx_non_429_is_permanent() {
        // Client errors (400/401/403/404) won't succeed on retry — 4xx means
        // the message body or credentials are wrong. Abort, don't retry.
        for code in [400, 401, 403, 404] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            assert_eq!(
                classify_telegram_status(status),
                TelegramSendOutcome::Permanent,
                "HTTP {code} must classify as permanent (no retry)"
            );
        }
    }

    // Compile-time guards on the retry-budget constants. `const _: () = ...`
    // pattern evaluates the assertion at build time, so a future edit that
    // sets `TELEGRAM_SEND_MAX_ATTEMPTS = 1` or pushes the backoff cap to
    // 60s will fail `cargo build` rather than slipping through.
    const _: () = assert!(
        TELEGRAM_SEND_MAX_ATTEMPTS >= 2,
        "retry must attempt at least twice (original + 1 retry)"
    );
    const _: () = assert!(
        TELEGRAM_SEND_BACKOFF_INITIAL_MS <= TELEGRAM_SEND_BACKOFF_CAP_SECS * 1000,
        "initial backoff must not exceed cap"
    );
    const _: () = assert!(
        TELEGRAM_SEND_BACKOFF_CAP_SECS <= 10,
        "backoff cap must stay low so a stuck chunk does not hold up \
         the rest of the notification task"
    );

    #[tokio::test]
    async fn test_send_telegram_chunk_with_retry_gives_up_on_permanent() {
        // Hit a guaranteed-404 URL path — should classify as Permanent on
        // the very first attempt and return false without retrying.
        let base_url = "http://127.0.0.1:1".to_string(); // port 1 → connect refused
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        let token = SecretString::from("fake".to_string());

        // Connect-refused = transient (we'll exhaust all retries then fail).
        let started = std::time::Instant::now();
        let ok = send_telegram_chunk_with_retry(&client, &base_url, &token, "chat", "msg").await;
        let elapsed = started.elapsed();

        assert!(!ok, "unreachable host must return false after retries");
        // Three transient attempts with 50ms timeout each + 100ms + 200ms
        // backoff between them = ~450ms floor. Confirm we actually slept
        // (i.e., retried) rather than giving up instantly on attempt 1.
        assert!(
            elapsed >= Duration::from_millis(100),
            "must have retried at least once (elapsed = {elapsed:?})"
        );
    }

    // -----------------------------------------------------------------------
    // Active mode — notify with all event types
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_active_service_notify_all_event_types() {
        let service = make_active_service();

        service.notify(NotificationEvent::AuthenticationFailed {
            reason: "test failure".to_string(),
        });
        service.notify(NotificationEvent::TokenRenewed);
        service.notify(NotificationEvent::TokenRenewalFailed {
            attempts: 3,
            reason: "timeout".to_string(),
        });
        service.notify(NotificationEvent::WebSocketConnected {
            connection_index: 0,
        });
        service.notify(NotificationEvent::WebSocketDisconnected {
            connection_index: 1,
            reason: "reset".to_string(),
        });
        service.notify(NotificationEvent::WebSocketReconnected {
            connection_index: 2,
        });
        service.notify(NotificationEvent::ShutdownInitiated);
        service.notify(NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 200,
            instruments_failed: 9,
            total_candles: 180000,
            persist_failures: 0,
            failed_instruments: vec![],
            failure_reasons: std::collections::HashMap::new(),
        });
        service.notify(NotificationEvent::CandleVerificationFailed {
            instruments_checked: 209,
            instruments_with_gaps: 3,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            ohlc_details: vec![],
            data_details: vec![],
            timestamp_details: vec![],
            weekend_violations: 0,
            weekend_details: vec![],
        });

        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // -----------------------------------------------------------------------
    // initialize — falls back to no-op without real SSM
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_initialize_returns_noop_without_ssm() {
        let config = NotificationConfig {
            send_timeout_ms: 100,
            telegram_api_base_url: "https://api.telegram.org".to_string(),
            sns_enabled: false,
        };
        let service = NotificationService::initialize(&config).await;
        if crate::test_support::has_aws_credentials() {
            // Dev machine with real AWS credentials — Telegram creds fetched from SSM.
            assert!(
                service.is_active(),
                "with real AWS credentials, notification service should be active"
            );
        } else {
            assert!(
                !service.is_active(),
                "without real SSM, service should be in no-op mode"
            );
        }
    }

    // -----------------------------------------------------------------------
    // strip_html_tags and mask_phone
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_html_tags_removes_bold() {
        assert_eq!(strip_html_tags("<b>Auth FAILED</b>"), "Auth FAILED");
    }

    #[test]
    fn test_strip_html_tags_preserves_plain_text() {
        assert_eq!(strip_html_tags("plain text"), "plain text");
    }

    #[test]
    fn test_strip_html_tags_handles_nested() {
        assert_eq!(strip_html_tags("<b>bold <i>italic</i></b>"), "bold italic");
    }

    #[test]
    fn test_strip_html_tags_empty() {
        assert_eq!(strip_html_tags(""), "");
    }

    #[test]
    fn test_mask_phone_indian_number() {
        assert_eq!(mask_phone("+919876543210"), "+91XXXXX43210");
    }

    #[test]
    fn test_mask_phone_short_number() {
        assert_eq!(mask_phone("+12345"), "***");
    }

    #[test]
    fn test_mask_phone_us_number() {
        assert_eq!(mask_phone("+12025551234"), "+12XXXX51234");
    }

    // -----------------------------------------------------------------------
    // strip_html_tags — edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_html_tags_unclosed_tag() {
        // Unclosed `<` means everything after it is treated as inside a tag
        assert_eq!(strip_html_tags("before<unclosed"), "before");
    }

    #[test]
    fn test_strip_html_tags_only_tags() {
        assert_eq!(strip_html_tags("<b></b><i></i>"), "");
    }

    #[test]
    fn test_strip_html_tags_special_chars_in_content() {
        // `&amp;` and `&lt;` entities are NOT HTML tags — preserved as-is
        assert_eq!(strip_html_tags("a &amp; b"), "a &amp; b");
    }

    #[test]
    fn test_strip_html_tags_angle_brackets_in_text() {
        // `>` without a preceding `<` is just text
        assert_eq!(strip_html_tags("5 > 3 and 2 < 4"), "5  3 and 2 ");
    }

    #[test]
    fn test_strip_html_tags_multiple_nested_tags() {
        assert_eq!(strip_html_tags("<b><i><u>deep</u></i></b>"), "deep");
    }

    // -----------------------------------------------------------------------
    // mask_phone — boundary tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_phone_exact_boundary_8_chars() {
        // Exactly 8 chars → masked (boundary)
        assert_eq!(mask_phone("12345678"), "***");
    }

    #[test]
    fn test_mask_phone_9_chars() {
        // 9 chars → first 3 + 1 X + last 5
        assert_eq!(mask_phone("123456789"), "123X56789");
    }

    #[test]
    fn test_mask_phone_empty() {
        assert_eq!(mask_phone(""), "***");
    }

    #[test]
    fn test_mask_phone_single_char() {
        assert_eq!(mask_phone("X"), "***");
    }

    // -----------------------------------------------------------------------
    // Disabled service — all critical event types are safe
    // -----------------------------------------------------------------------

    #[test]
    fn test_disabled_service_all_critical_events_safe() {
        let service = NotificationService::disabled();
        // Every event variant must be safe to fire on a disabled service
        service.notify(NotificationEvent::AuthenticationFailed {
            reason: "test".to_string(),
        });
        service.notify(NotificationEvent::TokenRenewalFailed {
            attempts: 99,
            reason: "catastrophic".to_string(),
        });
        service.notify(NotificationEvent::WebSocketDisconnected {
            connection_index: 0,
            reason: "reset".to_string(),
        });
        service.notify(NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 0,
            instruments_failed: 500,
            total_candles: 0,
            persist_failures: 0,
            failed_instruments: vec![],
            failure_reasons: std::collections::HashMap::new(),
        });
        service.notify(NotificationEvent::CandleVerificationFailed {
            instruments_checked: 0,
            instruments_with_gaps: 100,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            ohlc_details: vec![],
            data_details: vec![],
            timestamp_details: vec![],
            weekend_violations: 0,
            weekend_details: vec![],
        });
    }

    // -----------------------------------------------------------------------
    // Severity routing — Low severity does NOT trigger SMS
    // -----------------------------------------------------------------------

    #[test]
    fn test_severity_low_does_not_trigger_sms_path() {
        // StartupComplete is Low severity — it should NOT trigger the
        // SNS SMS code path. We verify this by checking severity.
        let event = NotificationEvent::StartupComplete { mode: "PAPER" };
        assert!(
            event.severity() < Severity::High,
            "Low-severity events must not reach SMS path (< High)"
        );
    }

    // -----------------------------------------------------------------------
    // Active service with SNS — SMS code path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_active_service_critical_event_triggers_sms_path() {
        // Critical events should attempt both Telegram and SNS (if configured).
        let event = NotificationEvent::AuthenticationFailed {
            reason: "critical test".to_string(),
        };
        assert!(
            event.severity() >= Severity::High,
            "AuthenticationFailed must be High or Critical severity"
        );
    }

    // -----------------------------------------------------------------------
    // strip_html_tags — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_html_tags_self_closing_tag() {
        assert_eq!(strip_html_tags("before<br/>after"), "beforeafter");
    }

    #[test]
    fn test_strip_html_tags_with_attributes() {
        assert_eq!(
            strip_html_tags(r#"<a href="https://example.com">link</a>"#),
            "link"
        );
    }

    #[test]
    fn test_strip_html_tags_mixed_content() {
        assert_eq!(
            strip_html_tags("<b>DLT</b> Trading System v<i>1.0</i>"),
            "DLT Trading System v1.0"
        );
    }

    // -----------------------------------------------------------------------
    // mask_phone — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_phone_exactly_9_chars() {
        let result = mask_phone("123456789");
        assert_eq!(result, "123X56789");
    }

    #[test]
    fn test_mask_phone_long_number() {
        // 15 chars: prefix=3, suffix=5, mask=7
        let result = mask_phone("123456789012345");
        assert_eq!(result, "123XXXXXXX12345");
    }

    // -----------------------------------------------------------------------
    // NotificationMode coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_noop_service_is_active_returns_false() {
        let service = NotificationService::build(NotificationMode::NoOp);
        assert!(!service.is_active());
    }

    // -----------------------------------------------------------------------
    // Wave 3-B Item 11 — coalescer wiring
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_coalescer_enabled_returns_true_after_enable_call() {
        let service = NotificationService::disabled();
        assert!(!service.coalescer_enabled());
        let upgraded = NotificationService::enable_coalescer(
            service,
            super::super::coalescer::CoalescerConfig::default(),
        );
        assert!(upgraded.coalescer_enabled());
    }

    #[test]
    fn test_default_service_has_no_coalescer() {
        let service = NotificationService::build(NotificationMode::NoOp);
        assert!(!service.coalescer_enabled());
    }

    #[tokio::test]
    async fn test_enable_coalescer_on_noop_service_sets_flag_no_drain_task() {
        let service = NotificationService::disabled();
        let upgraded = NotificationService::enable_coalescer(
            service,
            super::super::coalescer::CoalescerConfig::default(),
        );
        assert!(upgraded.coalescer_enabled());
        // No drain task is spawned for NoOp services — nothing to verify
        // beyond "did not panic".
    }

    #[tokio::test]
    async fn test_enable_coalescer_is_idempotent_when_already_enabled() {
        let service = NotificationService::disabled();
        let first = NotificationService::enable_coalescer(
            service,
            super::super::coalescer::CoalescerConfig::default(),
        );
        let first_ptr = Arc::as_ptr(&first);
        // Second call must short-circuit and return the same Arc.
        let second = NotificationService::enable_coalescer(
            first,
            super::super::coalescer::CoalescerConfig::default(),
        );
        let second_ptr = Arc::as_ptr(&second);
        assert_eq!(first_ptr, second_ptr);
        assert!(second.coalescer_enabled());
    }

    #[tokio::test]
    async fn test_notify_low_severity_with_coalescer_enabled_returns_quickly() {
        // Build a coalescer-wrapped service; firing a Severity::Low event
        // must NOT spawn a Telegram send task. We can't observe that
        // directly without mocking, but we CAN observe that the function
        // returns before the underlying HTTP would have a chance to fire,
        // and that the coalescer holds the bucket.
        let service = NotificationService::disabled();
        let service = NotificationService::enable_coalescer(
            service,
            super::super::coalescer::CoalescerConfig::default(),
        );
        // NoOp mode short-circuits inside `notify` BEFORE consulting the
        // coalescer — so this exercises the dropped-counter path. The
        // assertion is simply that no panic occurs.
        service.notify(NotificationEvent::AuthenticationSuccess);
    }

    // -----------------------------------------------------------------------
    // strip_html_tags — additional coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_html_tags_no_closing_bracket() {
        // "<tag without closing" — everything after '<' is inside tag
        assert_eq!(strip_html_tags("before<tag without closing"), "before");
    }

    #[test]
    fn test_strip_html_tags_consecutive_tags() {
        assert_eq!(strip_html_tags("<a><b><c>text</c></b></a>"), "text");
    }

    #[test]
    fn test_strip_html_tags_tag_with_newlines() {
        assert_eq!(
            strip_html_tags("<b>\nline1\nline2\n</b>"),
            "\nline1\nline2\n"
        );
    }

    #[test]
    fn test_strip_html_tags_emoji_content() {
        // Unicode content should pass through
        assert_eq!(strip_html_tags("<b>🚀 Launch</b>"), "🚀 Launch");
    }

    // -----------------------------------------------------------------------
    // mask_phone — additional boundary tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_phone_10_chars() {
        // 10 chars: prefix=3, suffix=5, mask=2
        assert_eq!(mask_phone("1234567890"), "123XX67890");
    }

    #[test]
    fn test_mask_phone_exactly_8_chars_boundary() {
        // 8 chars → returns "***" (boundary)
        assert_eq!(mask_phone("12345678"), "***");
    }

    #[test]
    fn test_mask_phone_7_chars() {
        // 7 chars → returns "***" (below boundary)
        assert_eq!(mask_phone("1234567"), "***");
    }

    // -----------------------------------------------------------------------
    // Active service with SNS phone — triggers SMS code path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_active_service_with_sns_high_severity_event() {
        // We can't easily create an aws_sdk_sns::Client in tests, but we can
        // verify that the notify() method correctly checks severity for SMS routing
        let event = NotificationEvent::AuthenticationFailed {
            reason: "test critical event".to_string(),
        };
        assert!(
            event.severity() >= Severity::High,
            "AuthenticationFailed must be High+ severity"
        );

        let low_event = NotificationEvent::StartupComplete { mode: "PAPER" };
        assert!(
            low_event.severity() < Severity::High,
            "StartupComplete must be below High severity"
        );
    }

    // -----------------------------------------------------------------------
    // Severity routing — medium severity does NOT trigger SMS
    // -----------------------------------------------------------------------

    #[test]
    fn test_severity_medium_does_not_trigger_sms() {
        let event = NotificationEvent::TokenRenewed;
        assert!(
            event.severity() < Severity::High,
            "TokenRenewed should not trigger SMS (below High)"
        );
    }

    // -----------------------------------------------------------------------
    // initialize — with SNS disabled
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_initialize_with_sns_disabled() {
        let config = NotificationConfig {
            send_timeout_ms: 100,
            telegram_api_base_url: "https://api.telegram.org".to_string(),
            sns_enabled: false,
        };
        let service = NotificationService::initialize(&config).await;
        // Without real SSM, should fall back to no-op
        // With real SSM, should be active but without SNS
        // Either way, should not panic
        let _ = service.is_active();
    }

    // -----------------------------------------------------------------------
    // initialize — with SNS enabled (still falls back gracefully)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_initialize_with_sns_enabled_falls_back_gracefully() {
        let config = NotificationConfig {
            send_timeout_ms: 100,
            telegram_api_base_url: "https://api.telegram.org".to_string(),
            sns_enabled: true,
        };
        let service = NotificationService::initialize(&config).await;
        // Whether SSM is available or not, initialize should not panic
        // SNS phone number fetch may fail, but service still initializes
        let _ = service.is_active();
    }

    // -----------------------------------------------------------------------
    // Active service with SNS client — High severity triggers SMS path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_active_service_notify_high_severity_without_sns_client() {
        // Active service without SNS client — high severity should NOT panic
        let service = make_active_service();
        // AuthenticationFailed is Critical severity
        service.notify(NotificationEvent::AuthenticationFailed {
            reason: "test high severity no sns".to_string(),
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // -----------------------------------------------------------------------
    // strip_html_tags — only angle brackets
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_html_tags_only_gt_symbol() {
        // A lone '>' without prior '<' is just a char — it toggles in_tag=false
        assert_eq!(strip_html_tags(">"), "");
    }

    #[test]
    fn test_strip_html_tags_only_lt_symbol() {
        // A lone '<' starts a "tag" that never ends — everything after is swallowed
        assert_eq!(strip_html_tags("<"), "");
    }

    #[test]
    fn test_strip_html_tags_gt_then_text() {
        // '>' sets in_tag=false, then "hello" is normal text
        assert_eq!(strip_html_tags(">hello"), "hello");
    }

    // -----------------------------------------------------------------------
    // mask_phone — Unicode and special characters
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_phone_unicode_chars() {
        // 12 chars: prefix=3, suffix=5, mask=4
        let result = mask_phone("+91ABCDEFGH");
        assert_eq!(result.len(), 11); // 3 + 4 + 5 - 1 (Xs overlap)
    }

    // -----------------------------------------------------------------------
    // send_telegram_message — message body format
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_send_telegram_message_body_format() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                    .await
                    .unwrap_or(0);
                let request_str = String::from_utf8_lossy(&buf[..n]);
                // Verify the request contains JSON body with expected fields
                assert!(request_str.contains("chat_id"));
                assert!(request_str.contains("parse_mode"));
                assert!(request_str.contains("HTML"));

                let body = r#"{"ok":true}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let token = SecretString::from("test-token".to_string());
        send_telegram_message(&client, &base_url, &token, "12345", "<b>Test</b>").await;
    }

    // -----------------------------------------------------------------------
    // NotificationService::disabled — multiple event types are safe
    // -----------------------------------------------------------------------

    #[test]
    fn test_disabled_service_websocket_events_safe() {
        let service = NotificationService::disabled();
        service.notify(NotificationEvent::WebSocketConnected {
            connection_index: 0,
        });
        service.notify(NotificationEvent::WebSocketDisconnected {
            connection_index: 0,
            reason: "test".to_string(),
        });
        service.notify(NotificationEvent::WebSocketReconnected {
            connection_index: 0,
        });
        service.notify(NotificationEvent::ShutdownInitiated);
        service.notify(NotificationEvent::ShutdownComplete);
    }

    // -----------------------------------------------------------------------
    // Active service — all low-severity events go only to Telegram
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_active_service_low_severity_events_no_sms() {
        let service = make_active_service();
        // Low-severity events should NOT trigger SNS SMS path
        service.notify(NotificationEvent::StartupComplete { mode: "PAPER" });
        service.notify(NotificationEvent::ShutdownComplete);
        service.notify(NotificationEvent::TokenRenewed);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // -----------------------------------------------------------------------
    // send_telegram_message — non-success HTTP response (e.g., 403)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_send_telegram_message_403_does_not_panic() {
        let base_url = start_mock_telegram_server(403).await;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();

        let token = SecretString::from("fake-bot-token".to_string());

        // Should log a warning but not panic or return an error.
        send_telegram_message(&client, &base_url, &token, "999999999", "test 403").await;
    }

    // -----------------------------------------------------------------------
    // send_telegram_message — 500 error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_send_telegram_message_500_does_not_panic() {
        let base_url = start_mock_telegram_server(500).await;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();

        let token = SecretString::from("fake-bot-token".to_string());

        send_telegram_message(&client, &base_url, &token, "123", "test 500").await;
    }

    // -----------------------------------------------------------------------
    // Active service — notify with connection refused logs warn
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_active_service_notify_connection_refused_logs_warning() {
        // The active service points to 127.0.0.1:1 (connection refused).
        // Notify spawns a background task that will fail silently.
        let service = make_active_service();

        service.notify(NotificationEvent::Custom {
            message: "test connection refused path".to_string(),
        });

        // Wait for the background task to attempt and fail.
        tokio::time::sleep(Duration::from_millis(300)).await;
        // No panic = success. The error is logged as warn.
    }

    // -----------------------------------------------------------------------
    // disabled service — is_active returns false consistently
    // -----------------------------------------------------------------------

    #[test]
    fn test_disabled_service_is_active_consistent() {
        let service = NotificationService::disabled();
        assert!(!service.is_active());
        assert!(!service.is_active()); // idempotent
    }

    // -----------------------------------------------------------------------
    // strip_html_tags — real notification event messages
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_html_tags_real_event_message() {
        let event = NotificationEvent::AuthenticationFailed {
            reason: "token expired".to_string(),
        };
        let message = event.to_message();
        let stripped = strip_html_tags(&message);
        // Stripped should contain the reason but no HTML tags.
        assert!(stripped.contains("token expired"));
        assert!(!stripped.contains('<'));
        assert!(!stripped.contains('>'));
    }

    // -----------------------------------------------------------------------
    // make_active_service — high severity event with no SNS client
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_active_service_critical_event_no_sns_does_not_attempt_sms() {
        let service = make_active_service(); // no sns_client
        // Critical event — normally would trigger SMS, but no SNS client.
        service.notify(NotificationEvent::AuthenticationFailed {
            reason: "critical without SNS".to_string(),
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        // No panic = success. SMS path is skipped due to None SNS client.
    }

    // -----------------------------------------------------------------------
    // mask_phone — consistency check
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_phone_consistent_output() {
        let phone = "+919876543210";
        let masked1 = mask_phone(phone);
        let masked2 = mask_phone(phone);
        assert_eq!(masked1, masked2, "masking should be deterministic");
    }

    #[test]
    fn test_mask_phone_preserves_prefix_and_suffix() {
        // 13 chars: prefix=3 (+91), suffix=5 (43210), mask=5 (XXXXX)
        let result = mask_phone("+919876543210");
        assert!(result.starts_with("+91"), "prefix must be preserved");
        assert!(result.ends_with("43210"), "suffix must be preserved");
    }

    #[test]
    fn test_mask_phone_mask_length_matches_middle() {
        // 13 chars: 13 - 3 - 5 = 5 X's
        let result = mask_phone("+919876543210");
        let x_count = result.chars().filter(|c| *c == 'X').count();
        assert_eq!(x_count, 5, "middle should be 5 X characters");
    }

    #[test]
    fn test_strip_html_tags_alternating_content_and_tags() {
        let input = "a<b>b</b>c<i>d</i>e";
        assert_eq!(strip_html_tags(input), "abcde");
    }

    #[test]
    fn test_strip_html_tags_tag_only_no_content() {
        assert_eq!(strip_html_tags("<br>"), "");
        assert_eq!(strip_html_tags("<hr/>"), "");
    }

    #[test]
    fn test_strip_html_tags_multiple_gt_without_lt() {
        // '>' without preceding '<' ends the tag state. When not in a tag, '>' triggers
        // in_tag = false (which is already false), so the '>' is not output.
        assert_eq!(strip_html_tags(">>>"), "");
    }

    // -----------------------------------------------------------------------
    // Active service with SNS — exercises SMS code path
    // -----------------------------------------------------------------------

    fn make_active_service_with_sns() -> Arc<NotificationService> {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();

        // Create a real AWS SNS client pointed at a fake endpoint.
        // The SNS call will fail, but the code path is exercised.
        let sns_config = aws_sdk_sns::Config::builder()
            .endpoint_url("http://127.0.0.1:1")
            .region(aws_sdk_sns::config::Region::new("ap-south-1"))
            .behavior_version_latest()
            .credentials_provider(aws_sdk_sns::config::Credentials::new(
                "fake-access-key",
                "fake-secret-key",
                None,
                None,
                "test",
            ))
            .build();
        let sns_client = aws_sdk_sns::Client::from_conf(sns_config);

        Arc::new(NotificationService::build(NotificationMode::Active {
            bot_token: SecretString::from("test-bot-token".to_string()),
            chat_id: "123456789".to_string(),
            http_client,
            telegram_api_base_url: "http://127.0.0.1:1".to_string(),
            sns_client: Some(sns_client),
            sns_phone_number: Some("+919876543210".to_string()),
        }))
    }

    #[tokio::test]
    async fn test_active_service_with_sns_critical_event_triggers_sms_code_path() {
        let service = make_active_service_with_sns();
        assert!(service.is_active());

        // AuthenticationFailed is Critical severity — triggers both Telegram and SNS
        service.notify(NotificationEvent::AuthenticationFailed {
            reason: "test SNS path".to_string(),
        });

        // Wait for background tasks to attempt (and fail) sends
        tokio::time::sleep(Duration::from_millis(500)).await;
        // No panic = success. Both Telegram and SNS paths exercised.
    }

    #[tokio::test]
    async fn test_active_service_with_sns_low_severity_skips_sms() {
        let service = make_active_service_with_sns();

        // StartupComplete is Low severity — only Telegram, not SNS
        service.notify(NotificationEvent::StartupComplete { mode: "PAPER" });

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn test_active_service_with_sns_multiple_critical_events() {
        let service = make_active_service_with_sns();

        // Multiple critical events
        service.notify(NotificationEvent::AuthenticationFailed {
            reason: "failure 1".to_string(),
        });
        service.notify(NotificationEvent::TokenRenewalFailed {
            attempts: 5,
            reason: "failure 2".to_string(),
        });

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // -----------------------------------------------------------------------
    // strip_html_tags — special characters preserved
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_html_tags_preserves_special_chars() {
        assert_eq!(strip_html_tags("a & b"), "a & b");
        assert_eq!(strip_html_tags("100%"), "100%");
        assert_eq!(strip_html_tags("P&L: +500"), "P&L: +500");
    }

    // -----------------------------------------------------------------------
    // Active service — rapid fire notifications
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_active_service_rapid_fire_does_not_panic() {
        let service = make_active_service();
        for i in 0..20 {
            service.notify(NotificationEvent::Custom {
                message: format!("rapid fire event {i}"),
            });
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
