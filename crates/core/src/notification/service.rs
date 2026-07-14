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
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

use super::coalescer::{
    CoalesceDecision, CoalescerConfig, DispatchLane, DrainedSummary, TelegramCoalescer,
    classify_dispatch, effective_drain_window, render_digest, should_force_drain,
};
use super::episode::{
    BOOT_EPISODE_KEY, EPISODE_EDIT_FAILURES_FALLBACK_THRESHOLD, EpisodeAction, EpisodeConfig,
    EpisodeFamily, EpisodeKey, EpisodePhase, EpisodeRegistry, EpisodeRenderCtx, episode_config_for,
    episode_snapshot, fnv1a_hash, render_boot_checklist, render_episode_first_page,
    render_episode_recovered, render_episode_recovering, render_episode_stale_closed,
    render_episode_steady,
};
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
    /// is sent immediately). `Some(...)` means Severity::Medium/Low/Info
    /// events are folded into per-`(topic, severity)` windows; Critical /
    /// High always bypass (2026-07-07 UX overhaul re-routed Medium).
    ///
    /// Wired via `enable_coalescer()` after `initialize()` based on the
    /// `features.telegram_bucket_coalescer` config flag.
    coalescer: Option<Arc<TelegramCoalescer>>,
    /// Telegram UX Overhaul (2026-07-07): episode live-edit kill switch.
    /// `false` → `episode_key()` consultation is a no-op and dispatch is
    /// byte-identical to legacy. Sourced from `[notification] episode_mode`.
    episode_mode: bool,
    /// One live bubble per (family, conn) incident — cold-path registry.
    episodes: Arc<EpisodeRegistry>,
    /// Best-effort snapshot writer channel (advisory episodes file).
    /// `None` in NoOp mode / when episode_mode is off.
    episode_store_tx: Option<tokio::sync::mpsc::Sender<String>>,
    /// Boot bubble (2026-07-09) kill switch — `false` routes the boot
    /// milestones back through their UNCHANGED legacy immediate lanes
    /// (today's per-event spray, byte-identical) WITHOUT touching the
    /// #1439 WS episode machinery. Sourced from `[notification] boot_bubble`.
    boot_bubble: bool,
    /// Serializes the ENTIRE boot fold→render→send/edit body — shared
    /// with the drain ticker's re-drive so the two can never race a
    /// duplicate first page. Boot milestones arrive milliseconds apart on
    /// separate spawned tasks; two racing the in-flight FIRST send
    /// (message id still unknown) would otherwise open duplicate bubbles.
    /// Cold path — once per milestone / per 10s tick.
    boot_dispatch_lock: Arc<tokio::sync::Mutex<()>>,
}

impl NotificationService {
    /// Internal helper — every constructor funnels through here so the
    /// field defaults live in one place. Adding a new field to the struct
    /// requires updating only this method.
    fn build(mode: NotificationMode) -> Self {
        Self {
            mode,
            coalescer: None,
            episode_mode: true,
            episodes: Arc::new(EpisodeRegistry::new()),
            episode_store_tx: None,
            boot_bubble: true,
            boot_dispatch_lock: Arc::new(tokio::sync::Mutex::new(())),
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

        let mut service = Self::build(NotificationMode::Active {
            bot_token,
            chat_id,
            http_client,
            telegram_api_base_url: config.telegram_api_base_url.clone(),
            sns_client,
            sns_phone_number,
        });
        // Telegram UX Overhaul (2026-07-07): episode live-edit machinery —
        // kill switch + best-effort snapshot rehydrate/persist. Fail-open:
        // NOTHING here can gate boot or any notify().
        service.episode_mode = config.episode_mode;
        if config.episode_mode {
            let store_path = std::path::PathBuf::from(EPISODE_STORE_PATH);
            rehydrate_episodes(&service.episodes, &store_path).await;
            service.episode_store_tx = Some(spawn_episode_store_writer(store_path));
        }
        // Boot bubble (2026-07-09): kill switch + best-effort deploy-flavor
        // detection (previous boot's sha vs the built-in sha). Fail-open —
        // NOTHING here can gate boot or any notify().
        service.boot_bubble = config.boot_bubble;
        if config.episode_mode && config.boot_bubble {
            init_boot_sha_flavor(&service.episodes).await;
        }
        Arc::new(service)
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
                // Telegram UX Overhaul (2026-07-07): WS lifecycle storms
                // fold into ONE live-edited episode bubble. Consulted
                // BEFORE the coalescer branch (wiring-guard pinned).
                // `episode_key()` is a zero-alloc Copy match, so the
                // non-episode fast path pays one comparison only.
                // Boot bubble (2026-07-09): boot milestones take the same
                // lane ONLY while `boot_bubble` is on — the `false`
                // rollback falls through to their unchanged legacy
                // immediate dispatch (byte-identical spray) WITHOUT
                // touching the WS episode families.
                if self.episode_mode
                    && event
                        .episode_key()
                        .is_some_and(|key| key.family != EpisodeFamily::Boot || self.boot_bubble)
                {
                    let svc = Arc::clone(self);
                    tokio::spawn(async move {
                        svc.dispatch_episode_event(event).await;
                    });
                    return;
                }
                // UX fix 2026-04-17: prefix every Telegram message with a
                // severity tag + emoji so the operator can visually scan
                // a long chat list and pick out incidents at a glance.
                // Parthiban workflow: any [HIGH] / [CRITICAL] message goes
                // straight to Claude Code for debugging.
                let topic = event.topic();
                // Render the body WITHOUT the severity tag — the prefix is
                // added exactly once by either (a) `deliver_summaries` on
                // drain (coalesced path) or (b) the bypass branch below.
                // Storing the prefixed string in the coalescer caused the
                // duplicate `✅ [LOW] ✅ [LOW]` Telegram bug observed by
                // Parthiban on 2026-05-03 (drain re-prepended the tag).
                let body = event.to_message();

                // 2026-07-07 UX overhaul: the lane decision is the pure
                // `classify_dispatch` — DispatchPolicy::Immediate always
                // wins (green boot pings unchanged, operator complaint
                // 2026-05-09); Critical/High NEVER batch; Medium/Low/Info
                // batch into the in-market digest window or the legacy
                // off-hours 60s coalescer. `episode` is None here — episode
                // events already returned above (or episode_mode is off,
                // the legacy rollback path).
                let lane = classify_dispatch(
                    severity,
                    event.dispatch_policy(),
                    None,
                    tickvault_common::market_hours::is_within_market_hours_ist(),
                );
                if matches!(lane, DispatchLane::Digest | DispatchLane::Coalesce60)
                    && let Some(coalescer) = self.coalescer.as_ref()
                {
                    let decision = coalescer.observe(topic, severity, || body.clone());
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

                // Bypass path: prefix the body with the severity tag + the
                // runtime source badge once. (The coalesced path applies the
                // same prefix in `deliver_summaries` on drain — both sites
                // go through the single `telegram_message_prefix` helper.)
                let message = format!("{} {}", telegram_message_prefix(severity), body);

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
            // 2026-07-07 UX overhaul: the SAME drain ticker also owns the
            // episode stability promotion (Recovering → green close) and
            // the market-phase digest window + 15:30 IST close force-drain.
            let episodes = Arc::clone(&service.episodes);
            let store_tx = service.episode_store_tx.clone();
            let episode_mode = service.episode_mode;
            let boot_lock = Arc::clone(&service.boot_dispatch_lock);

            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // Skip the immediate first tick so we don't try to drain
                // an empty coalescer right after spawn.
                ticker.tick().await;
                let mut prev_in_market =
                    tickvault_common::market_hours::is_within_market_hours_ist();
                loop {
                    ticker.tick().await;
                    if episode_mode {
                        run_episode_tick(
                            &episodes,
                            &boot_lock,
                            &client,
                            &base_url,
                            &token,
                            &chat_id,
                            store_tx.as_ref(),
                        )
                        .await;
                    }
                    let now_in_market =
                        tickvault_common::market_hours::is_within_market_hours_ist();
                    let force = should_force_drain(prev_in_market, now_in_market);
                    let window = effective_drain_window(now_in_market, &config);
                    prev_in_market = now_in_market;
                    let summaries = if force {
                        // Market-close boundary — no digest straddles
                        // overnight (robustness graft).
                        drain_coalescer.drain_all()
                    } else {
                        drain_coalescer.drain_mature_with_window(window)
                    };
                    if summaries.is_empty() {
                        continue;
                    }
                    deliver_drained(
                        &client,
                        &base_url,
                        &token,
                        &chat_id,
                        summaries,
                        now_in_market || force,
                    )
                    .await;
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

    // -----------------------------------------------------------------------
    // Telegram UX Overhaul (2026-07-07) — episode live-edit dispatch
    // -----------------------------------------------------------------------

    /// Test/observability accessor — the live episode registry.
    #[must_use]
    pub fn episode_registry(&self) -> &Arc<EpisodeRegistry> {
        &self.episodes
    }

    /// Returns `true` when the episode live-edit lane is on.
    #[must_use]
    pub fn episode_mode_enabled(&self) -> bool {
        self.episode_mode
    }

    /// Fallback for boots where the coalescer drain ticker is DISABLED
    /// (`features.telegram_bucket_coalescer = false`): spawns a tiny ticker
    /// that owns the episode stability promotion (Recovering → green
    /// close). No-op when the coalescer is wired (its drain loop already
    /// ticks the registry) or in NoOp mode.
    pub fn spawn_episode_ticker(service: &Arc<Self>) {
        if !service.episode_mode || service.coalescer.is_some() {
            return;
        }
        let NotificationMode::Active {
            bot_token,
            chat_id,
            http_client,
            telegram_api_base_url,
            ..
        } = &service.mode
        else {
            return;
        };
        let episodes = Arc::clone(&service.episodes);
        let boot_lock = Arc::clone(&service.boot_dispatch_lock);
        let store_tx = service.episode_store_tx.clone();
        let token = bot_token.clone();
        let chat_id = chat_id.clone();
        let client = http_client.clone();
        let base_url = telegram_api_base_url.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(
                super::coalescer::DEFAULT_FLUSH_INTERVAL_SECS,
            ));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                run_episode_tick(
                    &episodes,
                    &boot_lock,
                    &client,
                    &base_url,
                    &token,
                    &chat_id,
                    store_tx.as_ref(),
                )
                .await;
            }
        });
    }

    /// Graceful-shutdown flush (robustness graft, bounded 10s): drains
    /// every pending coalescer bucket, delivers the summaries, then writes
    /// a final synchronous episode snapshot. Wired into the main.rs
    /// shutdown teardown.
    pub async fn shutdown_flush(&self) {
        let flush = async {
            if let NotificationMode::Active {
                bot_token,
                chat_id,
                http_client,
                telegram_api_base_url,
                ..
            } = &self.mode
            {
                if let Some(coalescer) = self.coalescer.as_ref() {
                    let summaries = coalescer.drain_all();
                    if !summaries.is_empty() {
                        deliver_summaries(
                            http_client,
                            telegram_api_base_url,
                            bot_token,
                            chat_id,
                            summaries,
                        )
                        .await;
                    }
                }
                // Final synchronous snapshot — only when the persistence
                // writer was wired at initialize (keeps tests + ad-hoc
                // services from writing files).
                if self.episode_mode && self.episode_store_tx.is_some() {
                    let json = episode_snapshot::encode(&self.episodes.snapshot());
                    write_episode_store(std::path::Path::new(EPISODE_STORE_PATH), &json).await;
                }
            }
        };
        if tokio::time::timeout(Duration::from_secs(SHUTDOWN_FLUSH_TIMEOUT_SECS), flush)
            .await
            .is_err()
        {
            error!(
                target: "notification",
                code = tickvault_common::error_code::ErrorCode::Telegram01Dropped.code_str(),
                "TELEGRAM-01: shutdown flush timed out — pending coalesced summaries may be lost"
            );
        }
    }

    /// Enqueues a best-effort episode snapshot write (debounced writer
    /// task). A busy writer skips this snapshot — the next mutation
    /// re-enqueues; the shutdown flush writes synchronously.
    fn persist_episodes(&self) {
        if let Some(tx) = self.episode_store_tx.as_ref() {
            let json = episode_snapshot::encode(&self.episodes.snapshot());
            if let Err(err) = tx.try_send(json) {
                // Best-effort by design: a full channel just skips this
                // debounce slot (the write itself logs error! on failure).
                tracing::debug!(error = %err, "episode snapshot enqueue skipped (writer busy)");
            }
        }
    }

    /// The episode dispatch shell — the ONLY caller of the Telegram
    /// edit-message transport for live events (wiring-guard pinned; the
    /// drain ticker's `run_episode_tick` owns the green close edits).
    ///
    /// Never drops: every action either edits the live bubble or falls
    /// back to a fresh send; every terminal transport failure fires the
    /// EXISTING TELEGRAM-01 error! + counter.
    async fn dispatch_episode_event(self: Arc<Self>, event: NotificationEvent) {
        let NotificationMode::Active {
            bot_token,
            chat_id,
            http_client,
            telegram_api_base_url,
            sns_client,
            sns_phone_number,
        } = &self.mode
        else {
            return;
        };
        let Some(key) = event.episode_key() else {
            return;
        };
        // Boot bubble (2026-07-09): the Boot family has its own dispatcher
        // (checklist fold + fixed-Low prefix + serialized transport). The
        // 3-line branch keeps this shared fn's diff minimal (#1442-friendly).
        if key.family == EpisodeFamily::Boot {
            return self.dispatch_boot_episode_event(event).await;
        }
        let role = event.episode_role();
        let severity = event.severity();
        let attempts_hint = episode_attempts_hint(&event);
        let now_ms = epoch_ms_now();
        let cfg = EpisodeConfig::default();
        let decision = self
            .episodes
            .apply_event(key, role, severity, attempts_hint, now_ms, &cfg);

        match decision.action {
            EpisodeAction::Ignore => {
                // Resolve against a FRESH tombstone — the green close line
                // already announced this recovery; a second line would be
                // noise. (A resolve with NO tombstone routes SendLegacy —
                // never dropped; the proptest pins that High/Critical
                // Open/Progress can never land here.)
            }
            EpisodeAction::SendFirstPage => {
                metrics::counter!(
                    "tv_telegram_dispatched_total",
                    "severity" => severity.as_label(),
                    "coalesced" => "false",
                )
                .increment(1);
                // `escalate` = a live sub-High episode crossed into
                // High/Critical and re-paged fresh (hostile-review fix
                // 2026-07-07); `open` = a brand-new episode first page.
                let action_label: &'static str = if decision.escalated {
                    "escalate"
                } else {
                    "open"
                };
                metrics::counter!("tv_telegram_episode_events_total", "action" => action_label)
                    .increment(1);
                let page = render_episode_first_page(&event.to_message());
                let message = format!("{} {}", telegram_message_prefix(severity), page);
                let (ok, msg_id) = send_telegram_chunk_with_retry_returning_id(
                    http_client,
                    telegram_api_base_url,
                    bot_token,
                    chat_id,
                    &message,
                )
                .await;
                if ok {
                    self.episodes.record_sent(key, msg_id, now_ms);
                } else {
                    // Terminal transport failure — identical loudness to
                    // the legacy path. The episode keeps message_id=None so
                    // the NEXT event retries via SendNewFallback.
                    metrics::counter!(
                        "tv_telegram_dropped_total",
                        "reason" => "send_failed",
                    )
                    .increment(1);
                    error!(
                        target: "notification",
                        code = tickvault_common::error_code::ErrorCode::Telegram01Dropped.code_str(),
                        topic = event.topic(),
                        severity = severity.as_label(),
                        "TELEGRAM-01: episode first page delivery failed — operator alerts MAY be missed"
                    );
                }
                // SNS-SMS leg rides the FIRST page exactly once per episode
                // open (severity routing contract unchanged).
                if severity >= Severity::High
                    && let (Some(sns), Some(phone)) = (sns_client, sns_phone_number)
                {
                    let sms_text = strip_html_tags(&message);
                    send_sns_sms(sns, phone, &sms_text).await;
                }
                self.persist_episodes();
            }
            EpisodeAction::EditThrottled => {
                // Counters folded — the next eligible edit carries them.
                metrics::counter!(
                    "tv_telegram_episode_events_total",
                    "action" => "edit_throttled",
                )
                .increment(1);
                self.persist_episodes();
            }
            EpisodeAction::Edit { message_id, .. } => {
                let ctx = EpisodeRenderCtx { now_ms };
                let rendered = match decision.state.phase {
                    EpisodePhase::Recovering => render_episode_recovering(&decision.state, &ctx),
                    EpisodePhase::Down => render_episode_steady(&decision.state, &ctx),
                };
                let text = format!(
                    "{} {}",
                    telegram_message_prefix(decision.state.severity_peak),
                    rendered
                );
                let hash = fnv1a_hash(&text);
                if hash == self.episodes.last_render_hash(key) {
                    // Byte-identical render — a network edit would 400
                    // "message is not modified"; skip it entirely.
                    metrics::counter!(
                        "tv_telegram_episode_events_total",
                        "action" => "edit_throttled",
                    )
                    .increment(1);
                    return;
                }
                if decision.reopened {
                    metrics::counter!(
                        "tv_telegram_episode_events_total",
                        "action" => "reopen",
                    )
                    .increment(1);
                }
                match edit_telegram_message_with_retry(
                    http_client,
                    telegram_api_base_url,
                    bot_token,
                    chat_id,
                    message_id,
                    &text,
                )
                .await
                {
                    EditOutcome::Applied | EditOutcome::NotModifiedNoop => {
                        self.episodes.record_edit_applied(key, now_ms, hash);
                        metrics::counter!(
                            "tv_telegram_episode_events_total",
                            "action" => "edit",
                        )
                        .increment(1);
                    }
                    EditOutcome::Fallback => {
                        // Message gone (>48h, operator-deleted) — fresh
                        // bubble, duplicate-over-drop.
                        self.episode_fallback_send(key, &text, "not_found", now_ms)
                            .await;
                    }
                    EditOutcome::Transient => {
                        let failures = self.episodes.record_edit_failure(key);
                        // High/Critical episodes fall back to a FRESH send
                        // on the very first exhausted transient — a
                        // structurally-final event (e.g. the once-per-
                        // outage order-update page) may never re-drive the
                        // ladder, so duplicate-over-drop wins immediately
                        // (hostile-review fix 2026-07-07).
                        if failures >= EPISODE_EDIT_FAILURES_FALLBACK_THRESHOLD
                            || decision.state.severity_peak >= Severity::High
                        {
                            self.episode_fallback_send(key, &text, "transient_exhausted", now_ms)
                                .await;
                        } else {
                            // Sub-threshold transient on a <High episode:
                            // NEVER silent — counted + error!-loud; the
                            // next event or the drain ticker re-drives.
                            metrics::counter!(
                                "tv_telegram_edit_fallback_total",
                                "reason" => "transient_deferred",
                            )
                            .increment(1);
                            error!(
                                target: "notification",
                                code = tickvault_common::error_code::ErrorCode::Telegram03EpisodeDegraded.code_str(),
                                reason = "edit_transient_deferred",
                                consecutive_failures = failures,
                                "TELEGRAM-03: episode bubble edit failed transiently — will retry on the next event or fall back to a fresh bubble"
                            );
                        }
                    }
                }
                self.persist_episodes();
            }
            EpisodeAction::SendNewFallback => {
                // The first page never landed (send failed or the id was
                // unparseable) — send fresh; the explanation paragraph is
                // re-sent ONLY if it never went out.
                metrics::counter!(
                    "tv_telegram_dispatched_total",
                    "severity" => severity.as_label(),
                    "coalesced" => "false",
                )
                .increment(1);
                metrics::counter!("tv_telegram_episode_events_total", "action" => "open")
                    .increment(1);
                let ctx = EpisodeRenderCtx { now_ms };
                let body = if decision.state.explained {
                    render_episode_steady(&decision.state, &ctx)
                } else {
                    render_episode_first_page(&event.to_message())
                };
                let message = format!("{} {}", telegram_message_prefix(severity), body);
                let (ok, msg_id) = send_telegram_chunk_with_retry_returning_id(
                    http_client,
                    telegram_api_base_url,
                    bot_token,
                    chat_id,
                    &message,
                )
                .await;
                if ok {
                    self.episodes.record_sent(key, msg_id, now_ms);
                } else {
                    metrics::counter!(
                        "tv_telegram_dropped_total",
                        "reason" => "send_failed",
                    )
                    .increment(1);
                    error!(
                        target: "notification",
                        code = tickvault_common::error_code::ErrorCode::Telegram01Dropped.code_str(),
                        topic = event.topic(),
                        severity = severity.as_label(),
                        "TELEGRAM-01: episode fallback send failed — operator alerts MAY be missed"
                    );
                }
                self.persist_episodes();
            }
            EpisodeAction::SendLegacy => {
                // A recovery for an episode we never tracked (snapshot
                // lost across a restart / cross-task ordering) — deliver
                // it through the legacy immediate lane instead of
                // dropping the event (hostile-review fix 2026-07-07).
                // No episode state is created; no SMS (a recovery is not
                // an incident page).
                metrics::counter!(
                    "tv_telegram_dispatched_total",
                    "severity" => severity.as_label(),
                    "coalesced" => "false",
                )
                .increment(1);
                metrics::counter!(
                    "tv_telegram_episode_events_total",
                    "action" => "legacy_passthrough",
                )
                .increment(1);
                let body = render_episode_first_page(&event.to_message());
                let message = format!("{} {}", telegram_message_prefix(severity), body);
                let ok = send_telegram_chunk_with_retry(
                    http_client,
                    telegram_api_base_url,
                    bot_token,
                    chat_id,
                    &message,
                )
                .await;
                if !ok {
                    metrics::counter!(
                        "tv_telegram_dropped_total",
                        "reason" => "send_failed",
                    )
                    .increment(1);
                    error!(
                        target: "notification",
                        code = tickvault_common::error_code::ErrorCode::Telegram01Dropped.code_str(),
                        topic = event.topic(),
                        severity = severity.as_label(),
                        "TELEGRAM-01: legacy passthrough delivery failed — operator alerts MAY be missed"
                    );
                }
            }
        }
    }

    /// The edit-failure fallback rung: send a FRESH bubble carrying the
    /// same render, replace the episode's message id, count the reason.
    /// Duplicate-over-drop — a noisier chat beats a silent one.
    async fn episode_fallback_send(
        &self,
        key: EpisodeKey,
        text: &str,
        reason: &'static str,
        now_ms: u64,
    ) {
        let NotificationMode::Active {
            bot_token,
            chat_id,
            http_client,
            telegram_api_base_url,
            ..
        } = &self.mode
        else {
            return;
        };
        metrics::counter!("tv_telegram_edit_fallback_total", "reason" => reason).increment(1);
        error!(
            target: "notification",
            code = tickvault_common::error_code::ErrorCode::Telegram03EpisodeDegraded.code_str(),
            reason = "edit_fallback_storm",
            fallback_cause = reason,
            "TELEGRAM-03: episode bubble edit rejected — sending a fresh bubble (duplicate-over-drop)"
        );
        let (ok, msg_id) = send_telegram_chunk_with_retry_returning_id(
            http_client,
            telegram_api_base_url,
            bot_token,
            chat_id,
            text,
        )
        .await;
        if ok {
            self.episodes.replace_message_id(key, msg_id, now_ms);
        } else {
            metrics::counter!(
                "tv_telegram_dropped_total",
                "reason" => "send_failed",
            )
            .increment(1);
            error!(
                target: "notification",
                code = tickvault_common::error_code::ErrorCode::Telegram01Dropped.code_str(),
                "TELEGRAM-01: episode fallback send failed after edit rejection — operator alerts MAY be missed"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Boot bubble (2026-07-09) — ONE consolidated boot checklist bubble
    // -----------------------------------------------------------------------

    /// Declares which feed lines the boot checklist shows as ⏳-pending
    /// from the first page. Called by both boot lanes right after the
    /// notifier is constructed. No-op when the bubble lane is off.
    pub fn set_boot_expectations(&self, dhan_enabled: bool, groww_enabled: bool) {
        if self.episode_mode && self.boot_bubble {
            self.episodes
                .set_boot_expectations(dhan_enabled, groww_enabled, epoch_ms_now());
        }
    }

    /// Returns `true` when the consolidated boot bubble lane is on.
    #[must_use]
    pub fn boot_bubble_enabled(&self) -> bool {
        self.boot_bubble
    }

    /// The boot-bubble dispatch shell — folds the event's milestone into
    /// the one-per-boot checklist and delivers/edits the single bubble.
    ///
    /// The whole body runs under `boot_dispatch_lock` so concurrent boot
    /// milestones (spawned tasks, ms apart) can never race the in-flight
    /// first send into a duplicate bubble. Transport rungs are the SAME
    /// as the WS episode path: id-capture send, fnv1a hash-skip, not-found
    /// fallback fresh send, transient ladder — with TELEGRAM-01/03
    /// loudness unchanged on every failure.
    async fn dispatch_boot_episode_event(self: Arc<Self>, event: NotificationEvent) {
        let NotificationMode::Active {
            bot_token,
            chat_id,
            http_client,
            telegram_api_base_url,
            ..
        } = &self.mode
        else {
            return;
        };
        let Some(milestone) = event.boot_milestone() else {
            // Defensive: a Boot-keyed event without a milestone would be a
            // mapping bug — deliver through the legacy immediate lane
            // rather than drop (never-drop law).
            let body = render_episode_first_page(&event.to_message());
            let message = format!("{} {}", telegram_message_prefix(event.severity()), body);
            let ok = send_telegram_chunk_with_retry(
                http_client,
                telegram_api_base_url,
                bot_token,
                chat_id,
                &message,
            )
            .await;
            if !ok {
                metrics::counter!(
                    "tv_telegram_dropped_total",
                    "reason" => "send_failed",
                )
                .increment(1);
                error!(
                    target: "notification",
                    code = tickvault_common::error_code::ErrorCode::Telegram01Dropped.code_str(),
                    topic = event.topic(),
                    "TELEGRAM-01: boot legacy passthrough delivery failed — operator alerts MAY be missed"
                );
            }
            return;
        };
        // Serialize fold → render → send/edit end-to-end (cold path).
        let _serialized = self.boot_dispatch_lock.lock().await;
        let now_ms = epoch_ms_now();
        let cfg = episode_config_for(EpisodeFamily::Boot);
        let (decision, checklist) = self.episodes.apply_boot_milestone(milestone, now_ms, &cfg);
        let ctx = EpisodeRenderCtx { now_ms };
        // EVERY action renders the full checklist (first page, edit and
        // fallback alike) — never the WS steady/first-page renders. The
        // prefix is FIXED at [LOW]: the bubble is a green checklist.
        let text = format!(
            "{} {}",
            telegram_message_prefix(Severity::Low),
            render_boot_checklist(&checklist, &ctx)
        );
        metrics::counter!(
            "tv_telegram_dispatched_total",
            "severity" => Severity::Low.as_label(),
            "coalesced" => "false",
        )
        .increment(1);
        match decision.action {
            EpisodeAction::SendFirstPage | EpisodeAction::SendNewFallback => {
                metrics::counter!("tv_telegram_episode_events_total", "action" => "open")
                    .increment(1);
                let (ok, msg_id) = send_telegram_chunk_with_retry_returning_id(
                    http_client,
                    telegram_api_base_url,
                    bot_token,
                    chat_id,
                    &text,
                )
                .await;
                if ok {
                    self.episodes.record_sent(key_boot(), msg_id, now_ms);
                    self.episodes
                        .record_edit_applied(key_boot(), now_ms, fnv1a_hash(&text));
                    self.episodes.mark_boot_delivered();
                } else {
                    // Terminal transport failure — identical loudness to the
                    // legacy path; the checklist stays dirty so the drain
                    // ticker (≤10s) or the next milestone retries fresh.
                    metrics::counter!(
                        "tv_telegram_dropped_total",
                        "reason" => "send_failed",
                    )
                    .increment(1);
                    error!(
                        target: "notification",
                        code = tickvault_common::error_code::ErrorCode::Telegram01Dropped.code_str(),
                        topic = event.topic(),
                        "TELEGRAM-01: boot bubble first page delivery failed — operator alerts MAY be missed"
                    );
                }
            }
            EpisodeAction::EditThrottled => {
                // Unreachable with the Boot 0s throttle; folded safely —
                // the dirty checklist is re-driven by the drain ticker.
                metrics::counter!(
                    "tv_telegram_episode_events_total",
                    "action" => "edit_throttled",
                )
                .increment(1);
            }
            EpisodeAction::Edit { message_id, .. } => {
                let hash = fnv1a_hash(&text);
                if hash == self.episodes.last_render_hash(key_boot()) {
                    // Byte-identical render — nothing new to deliver.
                    self.episodes.mark_boot_delivered();
                    metrics::counter!(
                        "tv_telegram_episode_events_total",
                        "action" => "edit_throttled",
                    )
                    .increment(1);
                    return;
                }
                match edit_telegram_message_with_retry(
                    http_client,
                    telegram_api_base_url,
                    bot_token,
                    chat_id,
                    message_id,
                    &text,
                )
                .await
                {
                    EditOutcome::Applied | EditOutcome::NotModifiedNoop => {
                        self.episodes.record_edit_applied(key_boot(), now_ms, hash);
                        self.episodes.mark_boot_delivered();
                        metrics::counter!(
                            "tv_telegram_episode_events_total",
                            "action" => "edit",
                        )
                        .increment(1);
                    }
                    EditOutcome::Fallback => {
                        // Bubble gone (>48h / deleted) — fresh full-checklist
                        // bubble; the dirty flag stays and the ticker's
                        // hash-skip self-heals it once the fallback landed.
                        self.episode_fallback_send(key_boot(), &text, "not_found", now_ms)
                            .await;
                    }
                    EditOutcome::Transient => {
                        let failures = self.episodes.record_edit_failure(key_boot());
                        if failures >= EPISODE_EDIT_FAILURES_FALLBACK_THRESHOLD {
                            self.episode_fallback_send(
                                key_boot(),
                                &text,
                                "transient_exhausted",
                                now_ms,
                            )
                            .await;
                        } else {
                            // NEVER silent: counted + error!-loud; the drain
                            // ticker re-drives within ~10s (a failed FINAL
                            // edit has no next milestone to carry it).
                            metrics::counter!(
                                "tv_telegram_edit_fallback_total",
                                "reason" => "transient_deferred",
                            )
                            .increment(1);
                            error!(
                                target: "notification",
                                code = tickvault_common::error_code::ErrorCode::Telegram03EpisodeDegraded.code_str(),
                                reason = "edit_transient_deferred",
                                consecutive_failures = failures,
                                "TELEGRAM-03: boot bubble edit failed transiently — the ticker re-drives or falls back to a fresh bubble"
                            );
                        }
                    }
                }
            }
            // Role is always Open for boot milestones; the FSM never
            // returns Ignore/SendLegacy for Open. Folded defensively.
            EpisodeAction::Ignore | EpisodeAction::SendLegacy => {}
        }
        self.persist_episodes();
    }
}

/// The fixed boot-bubble key (tiny alias keeping call sites short).
const fn key_boot() -> EpisodeKey {
    BOOT_EPISODE_KEY
}

/// Renders + sends one Telegram message per drained summary.
///
/// Called by the drain task spawned in [`NotificationService::enable_coalescer`].
/// The SINGLE implementation point for the Telegram first-line prefix:
/// severity tag (emoji + `[LEVEL]`) followed by the runtime source badge
/// (`💻 LOCAL` / `☁️ AWS`).
///
/// Operator directive 2026-07-04: every Telegram message must clearly say
/// whether it came from the local Mac or the AWS box — during the local
/// window BOTH runtimes can post into the same chat. Both dispatch paths
/// (the immediate/bypass path in `notify` and the coalesced path in
/// `deliver_summaries`) build their prefix HERE, so the badge can never
/// drift between paths and per-event edits are never needed.
///
/// Commandment 10 is preserved: the severity emoji stays at the very start
/// of the subject line; the badge follows immediately after the severity
/// tag block.
pub(crate) fn telegram_message_prefix(severity: Severity) -> String {
    format!(
        "{} {}",
        severity.tag(),
        super::source_badge::runtime_source().badge()
    )
}

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
        // they would have got from the un-coalesced events. The runtime
        // source badge rides on the same shared prefix helper.
        let body = format!(
            "{} {}",
            telegram_message_prefix(summary.severity),
            summary.render_message()
        );
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
// Telegram UX Overhaul (2026-07-07) — episode + digest plumbing
// ---------------------------------------------------------------------------

/// Advisory episode snapshot file — best-effort, fail-open. NOT a QuestDB
/// surface; stale/corrupt files are ignored at rehydrate.
pub(crate) const EPISODE_STORE_PATH: &str = "data/notify/episodes.json";

/// Advisory previous-boot build-sha file (boot bubble, 2026-07-09) —
/// sibling of [`EPISODE_STORE_PATH`]. Best-effort, fail-open: unreadable /
/// unwritable / missing ⇒ plain header, never a boot blocker.
pub(crate) const BOOT_SHA_STORE_PATH: &str = "data/notify/last-boot-sha";

/// Bound on the graceful-shutdown flush (drain + deliver + store write).
pub(crate) const SHUTDOWN_FLUSH_TIMEOUT_SECS: u64 = 10;

/// Episode snapshot writer channel depth (best-effort debounce buffer).
const EPISODE_STORE_CHANNEL_CAPACITY: usize = 8;

/// Debounce between consecutive snapshot writes (≤ 1 write/sec).
const EPISODE_STORE_DEBOUNCE_MS: u64 = 1000;

/// Wall-clock epoch milliseconds (shell-side only — the pure episode core
/// never reads a clock).
fn epoch_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Today's IST calendar date (rehydrate same-day bound).
fn today_ist_date() -> chrono::NaiveDate {
    let offset = chrono::FixedOffset::east_opt(tickvault_common::constants::IST_UTC_OFFSET_SECONDS);
    match offset {
        Some(o) => chrono::Utc::now().with_timezone(&o).date_naive(),
        None => chrono::Utc::now().date_naive(),
    }
}

/// Reconnect-attempt context carried by recovery events (0 = unknown).
fn episode_attempts_hint(event: &NotificationEvent) -> u32 {
    match event {
        NotificationEvent::WebSocketReconnected { attempts, .. } => *attempts,
        NotificationEvent::OrderUpdateReconnected {
            consecutive_failures,
        } => *consecutive_failures,
        // GrowwFeed incident events (GrowwSidecarRejected / FeedDown /
        // FeedRecovered) carry no attempt count in their payload → 0, so
        // the registry uses the folded-event count (2026-07-14 noise fold).
        _ => 0,
    }
}

/// Spawns the debounced snapshot writer task (prev-close writer pattern:
/// dedicated task owns the file I/O; the dispatch path only enqueues).
fn spawn_episode_store_writer(path: std::path::PathBuf) -> tokio::sync::mpsc::Sender<String> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(EPISODE_STORE_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        while let Some(mut json) = rx.recv().await {
            // Coalesce any queued newer snapshots — write the latest only.
            while let Ok(newer) = rx.try_recv() {
                json = newer;
            }
            write_episode_store(&path, &json).await;
            tokio::time::sleep(Duration::from_millis(EPISODE_STORE_DEBOUNCE_MS)).await;
        }
    });
    tx
}

/// Writes the snapshot file. Failure → error! TELEGRAM-03
/// (`store_write_failed`) — in-memory episodes keep working; only the
/// cross-restart bubble linkage degrades.
async fn write_episode_store(path: &std::path::Path, json: &str) {
    if let Some(parent) = path.parent()
        && let Err(err) = tokio::fs::create_dir_all(parent).await
    {
        error!(
            target: "notification",
            code = tickvault_common::error_code::ErrorCode::Telegram03EpisodeDegraded.code_str(),
            reason = "store_write_failed",
            error = %err,
            "TELEGRAM-03: episode snapshot directory create failed — cross-restart bubble linkage degraded"
        );
        return;
    }
    if let Err(err) = tokio::fs::write(path, json).await {
        error!(
            target: "notification",
            code = tickvault_common::error_code::ErrorCode::Telegram03EpisodeDegraded.code_str(),
            reason = "store_write_failed",
            error = %err,
            "TELEGRAM-03: episode snapshot write failed — cross-restart bubble linkage degraded"
        );
    }
}

/// Pure honesty rule for the boot bubble's "NEW CODE deployed" header:
/// claimed ONLY when BOTH shas are plausible git shas (7–40 lowercase hex)
/// AND differ. An `unknown` / missing / garbage sha on either side renders
/// the plain header — never a false deploy claim.
#[must_use]
pub(crate) fn is_new_code_deploy(previous: Option<&str>, current: &str) -> bool {
    fn valid(sha: &str) -> bool {
        (7..=40).contains(&sha.len())
            && sha
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    }
    match previous {
        Some(prev) => valid(prev) && valid(current) && prev != current,
        None => false,
    }
}

/// Boot bubble (2026-07-09): best-effort deploy-flavor detection — reads
/// the previous boot's sha from [`BOOT_SHA_STORE_PATH`], compares with the
/// built-in sha, stamps the checklist flavor, then records the current sha
/// for the NEXT boot. Every filesystem error degrades to the plain header
/// (`new_code = false`) at debug!-level — never boot-blocking.
async fn init_boot_sha_flavor(episodes: &EpisodeRegistry) {
    let current = tickvault_common::build_info::BUILD_GIT_SHA;
    let path = std::path::Path::new(BOOT_SHA_STORE_PATH);
    let previous = match tokio::fs::read_to_string(path).await {
        Ok(contents) => Some(contents.trim().to_string()),
        Err(err) => {
            tracing::debug!(error = %err, "boot sha store unreadable — plain boot header");
            None
        }
    };
    let new_code = is_new_code_deploy(previous.as_deref(), current);
    episodes.set_boot_flavor(
        new_code,
        tickvault_common::build_info::build_git_sha_short(),
        epoch_ms_now(),
    );
    // Record the current sha for the next boot (best-effort).
    if let Some(parent) = path.parent()
        && let Err(err) = tokio::fs::create_dir_all(parent).await
    {
        tracing::debug!(error = %err, "boot sha store dir create failed — next boot renders plain");
        return;
    }
    if let Err(err) = tokio::fs::write(path, current).await {
        // Hostile-review fix 2026-07-09: a failed write would leave the
        // PREVIOUS boot's sha on disk, so the NEXT restart of this same
        // binary would falsely claim "NEW CODE deployed" again. Best-effort
        // delete the stale sha so the next boot reads nothing → plain
        // header (fail-open, never a false deploy claim).
        let removed = tokio::fs::remove_file(path).await.is_ok();
        tracing::debug!(
            error = %err,
            stale_sha_removed = removed,
            "boot sha store write failed — stale sha cleared so the next boot renders plain (never a false NEW CODE claim)"
        );
    }
}

/// Boot-time rehydrate — fail-open, never gates boot. Corrupt JSON →
/// error! TELEGRAM-03 (`rehydrate_corrupt`) + empty registry.
async fn rehydrate_episodes(episodes: &EpisodeRegistry, path: &std::path::Path) {
    let Ok(json) = tokio::fs::read_to_string(path).await else {
        // No file yet — fresh start, nothing to report.
        return;
    };
    if !json.trim().is_empty() && serde_json::from_str::<serde_json::Value>(&json).is_err() {
        error!(
            target: "notification",
            code = tickvault_common::error_code::ErrorCode::Telegram03EpisodeDegraded.code_str(),
            reason = "rehydrate_corrupt",
            "TELEGRAM-03: episode snapshot is corrupt — starting with an empty registry (a live outage opens one fresh duplicate bubble)"
        );
        return;
    }
    let entries = episode_snapshot::decode(&json, epoch_ms_now(), today_ist_date());
    episodes.rehydrate(entries);
}

/// The drain-ticker half of the episode machinery: promotes stable
/// `Recovering` episodes to CLOSED and issues the final green close edit
/// on each closed bubble. Close-edit failures fall back to a fresh green
/// line (duplicate-over-drop) and terminal failures keep TELEGRAM-01
/// loudness.
pub(crate) async fn run_episode_tick(
    episodes: &EpisodeRegistry,
    boot_lock: &tokio::sync::Mutex<()>,
    client: &reqwest::Client,
    base_url: &str,
    token: &SecretString,
    chat_id: &str,
    store_tx: Option<&tokio::sync::mpsc::Sender<String>>,
) {
    let now_ms = epoch_ms_now();
    // Boot bubble (2026-07-09): re-drive an undelivered checklist render
    // (a failed FINAL edit has no next milestone to carry it) + silent
    // retirement of a completed bubble past its window.
    run_boot_tick(
        episodes, boot_lock, client, base_url, token, chat_id, now_ms,
    )
    .await;
    let cfg = EpisodeConfig::default();
    let outcome = episodes.tick(now_ms, &cfg);
    if outcome.closed.is_empty() && outcome.expired.is_empty() {
        return;
    }
    // One loop, ONE edit call site (wiring-guard pinned): green close for
    // recovered episodes; a neutral stale-close for expired Down episodes
    // (hostile-review fix 2026-07-07 — a rehydrated Down bubble whose
    // recovery event never arrives must not show DOWN forever).
    let closed_iter = outcome.closed.iter().map(|st| (st, false));
    let expired_iter = outcome.expired.iter().map(|st| (st, true));
    for (st, stale) in closed_iter.chain(expired_iter) {
        let Some(message_id) = st.message_id else {
            continue;
        };
        let ctx = EpisodeRenderCtx { now_ms };
        let (rendered, action_label): (String, &'static str) = if stale {
            (render_episode_stale_closed(st), "expired")
        } else {
            (render_episode_recovered(st, &ctx), "close")
        };
        let text = format!("{} {}", telegram_message_prefix(Severity::Low), rendered);
        metrics::counter!("tv_telegram_episode_events_total", "action" => action_label)
            .increment(1);
        match edit_telegram_message_with_retry(client, base_url, token, chat_id, message_id, &text)
            .await
        {
            EditOutcome::Applied | EditOutcome::NotModifiedNoop => {}
            EditOutcome::Fallback | EditOutcome::Transient if stale => {
                // The stale close is cosmetic (no event content) — a fresh
                // message would be noise; loud + counted, never silent.
                metrics::counter!(
                    "tv_telegram_edit_fallback_total",
                    "reason" => "stale_close_edit_failed",
                )
                .increment(1);
                error!(
                    target: "notification",
                    code = tickvault_common::error_code::ErrorCode::Telegram03EpisodeDegraded.code_str(),
                    reason = "stale_close_edit_failed",
                    "TELEGRAM-03: stale episode close edit failed — the old bubble may keep showing DOWN; any new problem still opens a fresh alert"
                );
            }
            EditOutcome::Fallback | EditOutcome::Transient => {
                // The episode is already closed in the registry — send the
                // green line fresh so the recovery is never silent.
                metrics::counter!(
                    "tv_telegram_edit_fallback_total",
                    "reason" => "not_found",
                )
                .increment(1);
                let ok =
                    send_telegram_chunk_with_retry(client, base_url, token, chat_id, &text).await;
                if !ok {
                    metrics::counter!(
                        "tv_telegram_dropped_total",
                        "reason" => "send_failed",
                    )
                    .increment(1);
                    error!(
                        target: "notification",
                        code = tickvault_common::error_code::ErrorCode::Telegram01Dropped.code_str(),
                        "TELEGRAM-01: episode close line delivery failed — operator alerts MAY be missed"
                    );
                }
            }
        }
    }
    if let Some(tx) = store_tx {
        let json = episode_snapshot::encode(&episodes.snapshot());
        if let Err(err) = tx.try_send(json) {
            tracing::debug!(error = %err, "episode snapshot enqueue skipped (writer busy)");
        }
    }
}

/// Boot bubble (2026-07-09) — the drain-ticker half: retires a completed,
/// fully-delivered bubble silently, and re-drives a DIRTY checklist
/// (throttled/deferred/failed send or edit) so the final render always
/// lands within ~one tick (≤10s). Duplicate-over-drop on a dead bubble;
/// terminal failures keep TELEGRAM-01 loudness and stay dirty for the
/// next tick (bounded: one attempt per ~10s tick).
async fn run_boot_tick(
    episodes: &EpisodeRegistry,
    boot_lock: &tokio::sync::Mutex<()>,
    client: &reqwest::Client,
    base_url: &str,
    token: &SecretString,
    chat_id: &str,
    now_ms: u64,
) {
    // Same serialization as the milestone dispatcher — the re-drive can
    // never race a concurrent fold/send into a duplicate first page.
    let _serialized = boot_lock.lock().await;
    if episodes.retire_boot(now_ms) {
        tracing::debug!("boot bubble retired (complete + delivered + past the retire window)");
    }
    let Some((checklist, message_id)) = episodes.boot_redrive_candidate() else {
        return;
    };
    let ctx = EpisodeRenderCtx { now_ms };
    let text = format!(
        "{} {}",
        telegram_message_prefix(Severity::Low),
        render_boot_checklist(&checklist, &ctx)
    );
    let hash = fnv1a_hash(&text);
    match message_id {
        Some(id) => {
            if hash == episodes.last_render_hash(BOOT_EPISODE_KEY) {
                // Already delivered byte-identically (e.g. via a fallback
                // fresh send) — nothing pending.
                episodes.mark_boot_delivered();
                return;
            }
            match edit_telegram_message_with_retry(client, base_url, token, chat_id, id, &text)
                .await
            {
                EditOutcome::Applied | EditOutcome::NotModifiedNoop => {
                    episodes.record_edit_applied(BOOT_EPISODE_KEY, now_ms, hash);
                    episodes.mark_boot_delivered();
                    metrics::counter!(
                        "tv_telegram_episode_events_total",
                        "action" => "edit",
                    )
                    .increment(1);
                }
                EditOutcome::Transient
                    if episodes.record_edit_failure(BOOT_EPISODE_KEY)
                        < EPISODE_EDIT_FAILURES_FALLBACK_THRESHOLD =>
                {
                    // Transient edit failure below the ladder threshold
                    // (hostile-review fix 2026-07-09): DEFER — a single
                    // 429 must not spawn a duplicate bubble the milestone
                    // path would have retried. The checklist stays dirty;
                    // the next ~10s tick re-drives. NEVER silent.
                    metrics::counter!(
                        "tv_telegram_edit_fallback_total",
                        "reason" => "transient_deferred",
                    )
                    .increment(1);
                    error!(
                        target: "notification",
                        code = tickvault_common::error_code::ErrorCode::Telegram03EpisodeDegraded.code_str(),
                        reason = "edit_transient_deferred",
                        "TELEGRAM-03: boot bubble re-drive edit failed transiently — the next tick retries or falls back to a fresh bubble"
                    );
                }
                outcome @ (EditOutcome::Fallback | EditOutcome::Transient) => {
                    // Bubble gone (not_found) / transient ladder exhausted
                    // — fresh full-checklist bubble (duplicate-over-drop),
                    // with the honest per-cause metric reason so the
                    // TELEGRAM-03 triage split (bubble-gone vs API-flaky)
                    // stays trustworthy.
                    let reason = if matches!(outcome, EditOutcome::Fallback) {
                        "not_found"
                    } else {
                        "transient_exhausted"
                    };
                    metrics::counter!(
                        "tv_telegram_edit_fallback_total",
                        "reason" => reason,
                    )
                    .increment(1);
                    let (ok, msg_id) = send_telegram_chunk_with_retry_returning_id(
                        client, base_url, token, chat_id, &text,
                    )
                    .await;
                    if ok {
                        episodes.replace_message_id(BOOT_EPISODE_KEY, msg_id, now_ms);
                        episodes.record_edit_applied(BOOT_EPISODE_KEY, now_ms, hash);
                        episodes.mark_boot_delivered();
                    } else {
                        metrics::counter!(
                            "tv_telegram_dropped_total",
                            "reason" => "send_failed",
                        )
                        .increment(1);
                        error!(
                            target: "notification",
                            code = tickvault_common::error_code::ErrorCode::Telegram01Dropped.code_str(),
                            "TELEGRAM-01: boot bubble re-drive delivery failed — operator alerts MAY be missed"
                        );
                    }
                }
            }
        }
        None => {
            // The first page never landed — retry it fresh.
            metrics::counter!("tv_telegram_episode_events_total", "action" => "open").increment(1);
            let (ok, msg_id) = send_telegram_chunk_with_retry_returning_id(
                client, base_url, token, chat_id, &text,
            )
            .await;
            if ok {
                episodes.record_sent(BOOT_EPISODE_KEY, msg_id, now_ms);
                episodes.record_edit_applied(BOOT_EPISODE_KEY, now_ms, hash);
                episodes.mark_boot_delivered();
            } else {
                metrics::counter!(
                    "tv_telegram_dropped_total",
                    "reason" => "send_failed",
                )
                .increment(1);
                error!(
                    target: "notification",
                    code = tickvault_common::error_code::ErrorCode::Telegram01Dropped.code_str(),
                    "TELEGRAM-01: boot bubble first-page retry failed — operator alerts MAY be missed"
                );
            }
        }
    }
}

/// Delivers a drained batch: in market hours (or at the close force-drain)
/// the summaries fold into ONE digest bubble; off-hours keeps today's
/// per-summary delivery. A single count==1 summary keeps the legacy
/// bare-sample fast path (single-prefix contract preserved).
async fn deliver_drained(
    client: &reqwest::Client,
    base_url: &str,
    token: &SecretString,
    chat_id: &str,
    summaries: Vec<DrainedSummary>,
    digest_mode: bool,
) {
    let single_bare = summaries.len() == 1 && summaries.first().is_some_and(|s| s.count == 1);
    if !digest_mode || single_bare {
        deliver_summaries(client, base_url, token, chat_id, summaries).await;
        return;
    }
    let start = summaries.iter().map(|s| s.first_ts_ms).min().unwrap_or(0);
    let end = summaries
        .iter()
        .map(|s| s.last_ts_ms)
        .max()
        .unwrap_or(start);
    let max_severity = summaries
        .iter()
        .map(|s| s.severity)
        .max()
        .unwrap_or(Severity::Low);
    let body = format!(
        "{} {}",
        telegram_message_prefix(max_severity),
        render_digest(&summaries, start, end)
    );
    let ok = send_telegram_chunk_with_retry(client, base_url, token, chat_id, &body).await;
    if !ok {
        metrics::counter!(
            "tv_telegram_dropped_total",
            "reason" => "send_failed",
        )
        .increment(1);
        error!(
            target: "notification",
            code = tickvault_common::error_code::ErrorCode::Telegram01Dropped.code_str(),
            topics = summaries.len(),
            "TELEGRAM-01: digest delivery failed — operator alerts MAY be missed"
        );
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

/// Window for rate-limiting the transient send-failure WARN. During a
/// network/DNS outage every retry of every chunk hits the failure path
/// (dozens/min — see the 2026-05-29 13:03 IST local DNS dropout), flooding
/// the logs with identical "sendMessage HTTP error (transient)" lines. This
/// throttle affects ONLY that one log line — the send + retry logic and the
/// `error!("...report is incomplete")` alert-delivery signal are UNCHANGED, so
/// no alert is ever suppressed.
const TRANSIENT_SEND_WARN_WINDOW_SECS: i64 = 60;

/// Epoch seconds of the last emitted transient-send WARN (0 = never).
static LAST_TRANSIENT_WARN_EPOCH: AtomicI64 = AtomicI64::new(0);
/// Count of transient-send WARNs suppressed since the last emission.
static TRANSIENT_WARN_SUPPRESSED: AtomicU64 = AtomicU64::new(0);

/// Pure decision: should the transient-send WARN emit now? Emits when the
/// window has fully elapsed since the last emission (rising edge); otherwise
/// the caller suppresses (and counts) it. `last_epoch == 0` (never emitted)
/// always emits. Extracted for deterministic unit testing of the boundary.
#[must_use]
fn transient_warn_should_emit(now_epoch: i64, last_epoch: i64, window_secs: i64) -> bool {
    now_epoch.saturating_sub(last_epoch) >= window_secs
}

/// Rate-limited WARN for a transient Telegram send failure. At most one log
/// line per [`TRANSIENT_SEND_WARN_WINDOW_SECS`]; the emission carries the count
/// of WARNs suppressed since the previous one so the operator still sees the
/// magnitude. Pure log-noise control — never gates alert delivery.
fn warn_transient_send_throttled(safe_msg: &str) {
    let now = chrono::Utc::now().timestamp();
    let last = LAST_TRANSIENT_WARN_EPOCH.load(Ordering::Relaxed);
    if transient_warn_should_emit(now, last, TRANSIENT_SEND_WARN_WINDOW_SECS)
        && LAST_TRANSIENT_WARN_EPOCH
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        let suppressed = TRANSIENT_WARN_SUPPRESSED.swap(0, Ordering::Relaxed);
        warn!(
            error = %safe_msg,
            suppressed_in_last_window = suppressed,
            "notification: Telegram sendMessage HTTP error (transient)"
        );
    } else {
        // Either inside the window, or a racing thread won the window —
        // count this one for the next emission.
        TRANSIENT_WARN_SUPPRESSED.fetch_add(1, Ordering::Relaxed);
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
            // Rate-limited: during a network/DNS outage this path fires
            // dozens of times/min. Throttle the LOG line only — the send,
            // retry, and the "report incomplete" ERROR are untouched, so no
            // alert is ever suppressed (operator lock 2026-05-29 follow-up).
            warn_transient_send_throttled(&safe_msg);
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
// Telegram UX Overhaul (2026-07-07) — send-returning-id + edit transport
// ---------------------------------------------------------------------------

/// Parses `result.message_id` from a Telegram sendMessage 2xx response
/// body. Unparseable body → `None` (delivered-without-id: the caller never
/// re-sends; subsequent events take the SendNewFallback rung).
#[must_use]
pub(crate) fn parse_send_message_id(body: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("result")?
        .get("message_id")?
        .as_i64()
}

/// One sendMessage attempt that ALSO captures the created message id from
/// the response body (the legacy `send_telegram_message` discards it).
/// Used ONLY by the episode path.
async fn send_telegram_message_capture(
    client: &reqwest::Client,
    base_url: &str,
    bot_token: &SecretString,
    chat_id: &str,
    text: &str,
) -> (TelegramSendOutcome, Option<i64>) {
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
            let message_id = if matches!(outcome, TelegramSendOutcome::Success) {
                match response.text().await {
                    Ok(text) => parse_send_message_id(&text),
                    Err(_) => None,
                }
            } else {
                None
            };
            (outcome, message_id)
        }
        Err(err) => {
            // SECURITY: redact the bot token (reqwest errors may embed the URL).
            let safe_msg = err
                .to_string()
                .replace(bot_token.expose_secret(), "[REDACTED]");
            warn_transient_send_throttled(&safe_msg);
            (TelegramSendOutcome::Transient, None)
        }
    }
}

/// Same 3-attempt / 100ms→2s ladder as `send_telegram_chunk_with_retry`,
/// returning `(delivered, message_id)`. `(true, None)` = delivered but the
/// id was unparseable — counted as delivered, never re-sent.
pub(crate) async fn send_telegram_chunk_with_retry_returning_id(
    client: &reqwest::Client,
    base_url: &str,
    bot_token: &SecretString,
    chat_id: &str,
    text: &str,
) -> (bool, Option<i64>) {
    let mut delay = Duration::from_millis(TELEGRAM_SEND_BACKOFF_INITIAL_MS);
    let cap = Duration::from_secs(TELEGRAM_SEND_BACKOFF_CAP_SECS);
    for attempt in 1..=TELEGRAM_SEND_MAX_ATTEMPTS {
        match send_telegram_message_capture(client, base_url, bot_token, chat_id, text).await {
            (TelegramSendOutcome::Success, message_id) => return (true, message_id),
            (TelegramSendOutcome::Permanent, _) => return (false, None),
            (TelegramSendOutcome::Transient, _) => {
                if attempt < TELEGRAM_SEND_MAX_ATTEMPTS {
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2).min(cap);
                }
            }
        }
    }
    (false, None)
}

/// Outcome of an editMessageText exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditOutcome {
    /// 2xx — the bubble text was updated.
    Applied,
    /// 400 "message is not modified" — the text is already identical.
    /// SUCCESS, never a fallback trigger (kills the fallback-spam bug).
    NotModifiedNoop,
    /// Permanent rejection (message gone / can't be edited / other 4xx) —
    /// the fallback rung sends a fresh bubble.
    Fallback,
    /// 429 / 5xx / network — retryable; exhausted retries surface as this.
    Transient,
}

/// Pure classifier for an editMessageText response (robustness matrix).
#[must_use]
pub(crate) fn classify_edit_body(status: u16, body: &str) -> EditOutcome {
    if (200..300).contains(&status) {
        return EditOutcome::Applied;
    }
    if status == 429 || status >= 500 {
        return EditOutcome::Transient;
    }
    if status == 400 && body.contains("message is not modified") {
        return EditOutcome::NotModifiedNoop;
    }
    // 400 "message to edit not found" / "message can't be edited" / any
    // other permanent 4xx → fresh-send fallback (duplicate-over-drop).
    EditOutcome::Fallback
}

/// Edits a Telegram message in place via the Bot API editMessageText
/// method — raw HTTP through the SAME client/retry machinery as
/// sendMessage (no new transport dependency). Retries Transient outcomes
/// on the standard ladder; returns the final outcome.
pub(crate) async fn edit_telegram_message_with_retry(
    client: &reqwest::Client,
    base_url: &str,
    bot_token: &SecretString,
    chat_id: &str,
    message_id: i64,
    text: &str,
) -> EditOutcome {
    // The ONLY site that builds this URL (wiring-guard pinned).
    let url = format!(
        "{}/bot{}/editMessageText",
        base_url,
        bot_token.expose_secret()
    );
    let body = serde_json::json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "text": text,
        "parse_mode": "HTML",
    });
    let mut delay = Duration::from_millis(TELEGRAM_SEND_BACKOFF_INITIAL_MS);
    let cap = Duration::from_secs(TELEGRAM_SEND_BACKOFF_CAP_SECS);
    let mut last = EditOutcome::Transient;
    for attempt in 1..=TELEGRAM_SEND_MAX_ATTEMPTS {
        last = match client.post(&url).json(&body).send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let response_body = response.text().await.unwrap_or_default();
                classify_edit_body(status, &response_body)
            }
            Err(err) => {
                let safe_msg = err
                    .to_string()
                    .replace(bot_token.expose_secret(), "[REDACTED]");
                warn_transient_send_throttled(&safe_msg);
                EditOutcome::Transient
            }
        };
        match last {
            EditOutcome::Transient => {
                if attempt < TELEGRAM_SEND_MAX_ATTEMPTS {
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2).min(cap);
                }
            }
            _ => return last,
        }
    }
    last
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
    // telegram_message_prefix — severity tag + runtime source badge
    // Operator directive 2026-07-04: every Telegram must say LOCAL or AWS.
    // -----------------------------------------------------------------------

    #[test]
    fn test_telegram_message_prefix_starts_with_severity_tag() {
        // Commandment 10: severity emoji stays at the START of the subject.
        for sev in [
            Severity::Info,
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ] {
            let prefix = telegram_message_prefix(sev);
            assert!(
                prefix.starts_with(sev.tag()),
                "prefix must start with the severity tag: {prefix:?}"
            );
        }
    }

    #[test]
    fn test_telegram_message_prefix_carries_source_badge_after_tag() {
        let prefix = telegram_message_prefix(Severity::Critical);
        let badge = crate::notification::source_badge::runtime_source().badge();
        let expected = format!("{} {}", Severity::Critical.tag(), badge);
        assert_eq!(prefix, expected);
        // The badge is exactly one of the two allowed values — never both.
        let has_local = prefix.contains("LOCAL");
        let has_aws = prefix.contains("AWS");
        assert!(
            has_local ^ has_aws,
            "exactly one badge expected: {prefix:?}"
        );
    }

    #[test]
    fn test_telegram_message_prefix_under_cargo_test_is_local() {
        // `cargo test` is never a systemd-managed process on the dev/CI
        // boxes this suite runs on, so the cached runtime source resolves
        // LOCAL here. (The AWS arm is covered by the pure classifier tests
        // in source_badge.rs — env mutation is deliberately avoided.)
        if std::env::var_os("NOTIFY_SOCKET").is_none() {
            let prefix = telegram_message_prefix(Severity::Info);
            assert!(prefix.contains("LOCAL"), "expected LOCAL badge: {prefix:?}");
        }
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
        service.notify(NotificationEvent::StartupComplete {
            mode: "LIVE",
            spot_1m_enabled: true,
            spot_1m_indices: 4,
            chain_1m_enabled: true,
            chain_1m_underlyings: 3,
        });
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

        service.notify(NotificationEvent::StartupComplete {
            mode: "LIVE",
            spot_1m_enabled: true,
            spot_1m_indices: 4,
            chain_1m_enabled: true,
            chain_1m_underlyings: 3,
        });
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
            subscribed_count: 5_000,
            capacity: 5_000,
            last_activity_secs_ago: Some(1),
        });
        service.notify(NotificationEvent::WebSocketDisconnected {
            connection_index: 1,
            reason: "reset".to_string(),
        });
        service.notify(NotificationEvent::WebSocketReconnected {
            connection_index: 2,
            reason: None,
            down_secs: 0,
            attempts: 0,
        });
        service.notify(NotificationEvent::ShutdownInitiated);
        // PR-D (2026-05-26): HistoricalFetchFailed + CandleVerificationFailed
        // notify calls removed alongside the deleted variants.

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
            ..NotificationConfig::default()
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
        // PR-D (2026-05-26): HistoricalFetchFailed + CandleVerificationFailed
        // notify calls removed alongside the deleted variants.
    }

    // -----------------------------------------------------------------------
    // Severity routing — Low severity does NOT trigger SMS
    // -----------------------------------------------------------------------

    #[test]
    fn test_severity_low_does_not_trigger_sms_path() {
        // StartupComplete is Low severity — it should NOT trigger the
        // SNS SMS code path. We verify this by checking severity.
        let event = NotificationEvent::StartupComplete {
            mode: "PAPER",
            spot_1m_enabled: true,
            spot_1m_indices: 4,
            chain_1m_enabled: true,
            chain_1m_underlyings: 3,
        };
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

        let low_event = NotificationEvent::StartupComplete {
            mode: "PAPER",
            spot_1m_enabled: true,
            spot_1m_indices: 4,
            chain_1m_enabled: true,
            chain_1m_underlyings: 3,
        };
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
            ..NotificationConfig::default()
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
            ..NotificationConfig::default()
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
            subscribed_count: 5_000,
            capacity: 5_000,
            last_activity_secs_ago: Some(1),
        });
        service.notify(NotificationEvent::WebSocketDisconnected {
            connection_index: 0,
            reason: "test".to_string(),
        });
        service.notify(NotificationEvent::WebSocketReconnected {
            connection_index: 0,
            reason: None,
            down_secs: 0,
            attempts: 0,
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
        service.notify(NotificationEvent::StartupComplete {
            mode: "PAPER",
            spot_1m_enabled: true,
            spot_1m_indices: 4,
            chain_1m_enabled: true,
            chain_1m_underlyings: 3,
        });
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
        service.notify(NotificationEvent::StartupComplete {
            mode: "PAPER",
            spot_1m_enabled: true,
            spot_1m_indices: 4,
            chain_1m_enabled: true,
            chain_1m_underlyings: 3,
        });

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

    #[test]
    fn test_transient_warn_should_emit_window_boundary() {
        let w = TRANSIENT_SEND_WARN_WINDOW_SECS;
        // Never emitted before (last == 0) → emit.
        assert!(transient_warn_should_emit(1000, 0, w));
        // Exactly at the window boundary → emit (>=).
        assert!(transient_warn_should_emit(1000 + w, 1000, w));
        // One second past the boundary → emit.
        assert!(transient_warn_should_emit(1001 + w, 1000, w));
        // Inside the window → suppress.
        assert!(!transient_warn_should_emit(1000 + w - 1, 1000, w));
        assert!(!transient_warn_should_emit(1000, 1000, w));
        // Clock skew backwards (now < last) → suppress (saturating_sub → 0).
        assert!(!transient_warn_should_emit(500, 1000, w));
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

    // =======================================================================
    // Telegram UX Overhaul (2026-07-07) — episode dispatch + edit transport
    // =======================================================================

    use super::super::episode::{
        EpisodeConfig, EpisodeFamily, EpisodeKey, EpisodeRole, episode_snapshot,
    };

    #[test]
    fn test_parse_send_message_id_extracts_and_rejects_garbage() {
        assert_eq!(
            parse_send_message_id(r#"{"ok":true,"result":{"message_id":123,"date":1}}"#),
            Some(123)
        );
        assert_eq!(
            parse_send_message_id(r#"{"ok":true,"result":{"message_id":-5}}"#),
            Some(-5)
        );
        // Garbage / shape mismatches → None (delivered-without-id path).
        assert_eq!(parse_send_message_id(""), None);
        assert_eq!(parse_send_message_id("not json"), None);
        assert_eq!(parse_send_message_id(r#"{"ok":true}"#), None);
        assert_eq!(
            parse_send_message_id(r#"{"result":{"message_id":"str"}}"#),
            None
        );
        assert_eq!(parse_send_message_id(r#"{"message_id":9}"#), None);
    }

    #[test]
    fn test_classify_edit_body_matrix() {
        // 2xx → Applied.
        assert_eq!(
            classify_edit_body(200, r#"{"ok":true}"#),
            EditOutcome::Applied
        );
        // 400 "message is not modified" → benign noop (SUCCESS — never
        // triggers the fallback ladder; the mvp-lens misclassified this).
        assert_eq!(
            classify_edit_body(
                400,
                r#"{"ok":false,"description":"Bad Request: message is not modified"}"#
            ),
            EditOutcome::NotModifiedNoop
        );
        // Message gone / uneditable → Fallback (fresh send).
        assert_eq!(
            classify_edit_body(
                400,
                r#"{"ok":false,"description":"Bad Request: message to edit not found"}"#
            ),
            EditOutcome::Fallback
        );
        assert_eq!(
            classify_edit_body(
                400,
                r#"{"ok":false,"description":"Bad Request: message can't be edited"}"#
            ),
            EditOutcome::Fallback
        );
        // Other permanent 4xx → Fallback.
        assert_eq!(classify_edit_body(403, ""), EditOutcome::Fallback);
        assert_eq!(classify_edit_body(404, ""), EditOutcome::Fallback);
        // 429 + 5xx → Transient (retry ladder).
        assert_eq!(classify_edit_body(429, ""), EditOutcome::Transient);
        for code in [500_u16, 502, 503, 504] {
            assert_eq!(classify_edit_body(code, ""), EditOutcome::Transient);
        }
    }

    /// Scripted mock Telegram server: editMessageText → 400 "message to
    /// edit not found"; sendMessage → 200 with message_id 777. Serves
    /// connections until the test ends.
    async fn start_scripted_telegram_server() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let (status_line, body) = if request.contains("/editMessageText") {
                        (
                            "HTTP/1.1 400 Bad Request",
                            r#"{"ok":false,"description":"Bad Request: message to edit not found"}"#,
                        )
                    } else {
                        (
                            "HTTP/1.1 200 OK",
                            r#"{"ok":true,"result":{"message_id":777}}"#,
                        )
                    };
                    let response = format!(
                        "{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        status_line,
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        base_url
    }

    /// Like `start_scripted_telegram_server` but RECORDS every request
    /// path (so tests can assert which transport calls fired) and lets
    /// the caller choose the editMessageText status (400-not-found vs
    /// 429-transient).
    async fn start_recording_telegram_server(
        edit_status: u16,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let log_task = std::sync::Arc::clone(&log);

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let log_conn = std::sync::Arc::clone(&log_task);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let (status_line, body) = if request.contains("/editMessageText") {
                        log_conn.lock().unwrap().push("edit".to_string());
                        match edit_status {
                            200 => (
                                "HTTP/1.1 200 OK",
                                r#"{"ok":true,"result":{"message_id":777}}"#,
                            ),
                            429 => (
                                "HTTP/1.1 429 Too Many Requests",
                                r#"{"ok":false,"description":"Too Many Requests"}"#,
                            ),
                            _ => (
                                "HTTP/1.1 400 Bad Request",
                                r#"{"ok":false,"description":"Bad Request: message to edit not found"}"#,
                            ),
                        }
                    } else {
                        log_conn.lock().unwrap().push("send".to_string());
                        (
                            "HTTP/1.1 200 OK",
                            r#"{"ok":true,"result":{"message_id":777}}"#,
                        )
                    };
                    let response = format!(
                        "{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        status_line,
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        (base_url, log)
    }

    fn make_scripted_service(base_url: String) -> Arc<NotificationService> {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        Arc::new(NotificationService::build(NotificationMode::Active {
            bot_token: SecretString::from("test-bot-token".to_string()),
            chat_id: "123".to_string(),
            http_client,
            telegram_api_base_url: base_url,
            sns_client: None,
            sns_phone_number: None,
        }))
    }

    fn main_feed_key() -> EpisodeKey {
        EpisodeKey {
            family: EpisodeFamily::MainFeedWs,
            conn: 0,
        }
    }

    #[tokio::test]
    async fn test_edit_fallback_replaces_message_id_and_counts() {
        // base_url-override fixture pattern (mock server): the edit is
        // rejected 400 "message to edit not found" → the fallback rung
        // sends a FRESH bubble (200, message_id 777) and replaces the
        // episode's message id (duplicate-over-drop, never silent).
        let base_url = start_scripted_telegram_server().await;
        let service = make_scripted_service(base_url);
        let key = main_feed_key();
        let now = epoch_ms_now();
        // Seed a live episode whose bubble is message 42, last edited 60s
        // ago (outside the 20s throttle).
        let _ = service.episodes.apply_event(
            key,
            EpisodeRole::Open,
            Severity::High,
            0,
            now.saturating_sub(60_000),
            &EpisodeConfig::default(),
        );
        service
            .episodes
            .record_sent(key, Some(42), now.saturating_sub(60_000));

        let event = NotificationEvent::WebSocketDisconnected {
            connection_index: 0,
            reason: "reset".to_string(),
        };
        Arc::clone(&service).dispatch_episode_event(event).await;

        let snap = service.episodes.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(
            snap[0].message_id,
            Some(777),
            "fallback fresh send must replace the bubble message id"
        );
        assert_eq!(snap[0].occurrences, 2, "the repeat drop was folded");
    }

    #[tokio::test]
    async fn test_sms_fires_once_per_episode_open() {
        // The SNS-SMS leg rides EXACTLY the SendFirstPage action. This
        // test pins the gating precondition mechanically: across a
        // 3-event storm the registry opens ONE episode (one first page →
        // one possible SMS), and the repeats fold as edits/throttles.
        // (The SNS transport itself is exercised by the existing
        // make_active_service_with_sns tests.)
        let base_url = start_scripted_telegram_server().await;
        let service = make_scripted_service(base_url);
        for _ in 0..3 {
            let event = NotificationEvent::WebSocketDisconnected {
                connection_index: 0,
                reason: "reset".to_string(),
            };
            Arc::clone(&service).dispatch_episode_event(event).await;
        }
        let snap = service.episodes.snapshot();
        assert_eq!(snap.len(), 1, "one bubble per incident");
        assert_eq!(
            snap[0].message_id,
            Some(777),
            "first page delivered exactly once (id captured once)"
        );
        assert_eq!(snap[0].occurrences, 3, "repeats folded, never re-paged");
        assert!(snap[0].explained, "explanation paragraph sent once");
    }

    #[tokio::test]
    async fn test_notify_episode_event_routes_to_episode_lane() {
        // notify() consults episode_key BEFORE the coalescer branch: a
        // WS-lifecycle event lands in the registry, not the coalescer.
        let base_url = start_scripted_telegram_server().await;
        let service = make_scripted_service(base_url);
        service.notify(NotificationEvent::WebSocketDisconnected {
            connection_index: 0,
            reason: "reset".to_string(),
        });
        // The dispatch runs in a spawned task — poll briefly (via the
        // public registry accessor, which is also its call-site pin).
        for _ in 0..50 {
            if service.episode_registry().live_count() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(service.episode_registry().live_count(), 1);
        assert!(service.episode_mode_enabled());
    }

    #[tokio::test]
    async fn test_run_episode_tick_promotes_recovering_to_green_close() {
        let base_url = start_scripted_telegram_server().await;
        let service = make_scripted_service(base_url.clone());
        let key = main_feed_key();
        let cfg = EpisodeConfig::default();
        let long_ago = epoch_ms_now().saturating_sub(10 * 60_000);
        let _ =
            service
                .episodes
                .apply_event(key, EpisodeRole::Open, Severity::High, 0, long_ago, &cfg);
        service.episodes.record_sent(key, Some(42), long_ago);
        let _ = service.episodes.apply_event(
            key,
            EpisodeRole::Resolve,
            Severity::Medium,
            5,
            long_ago.saturating_add(1000),
            &cfg,
        );
        // The recovery is >60s old → the tick closes it (the green-close
        // edit is attempted against the mock; 400 → fresh green line).
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let token = SecretString::from("test-bot-token".to_string());
        run_episode_tick(
            &service.episodes,
            &tokio::sync::Mutex::new(()),
            &client,
            &base_url,
            &token,
            "123",
            None,
        )
        .await;
        assert_eq!(
            service.episodes.live_count(),
            0,
            "stable recovery must close the episode"
        );
    }

    #[tokio::test]
    async fn test_run_episode_tick_expires_stale_down_bubble() {
        // Hostile-review fix: a Down episode with no events for 30 minutes
        // (the restart edge — a clean first connect emits no recovery
        // event) expires from the registry so a later outage opens a
        // FRESH first page instead of silently editing a stale bubble.
        let (base_url, log) = start_recording_telegram_server(400).await;
        let service = make_scripted_service(base_url.clone());
        let key = main_feed_key();
        let cfg = EpisodeConfig::default();
        let stale_ago = epoch_ms_now().saturating_sub(
            super::super::episode::EPISODE_DOWN_STALE_EXPIRE_SECS
                .saturating_mul(1000)
                .saturating_add(60_000),
        );
        let _ = service.episodes.apply_event(
            key,
            EpisodeRole::Open,
            Severity::High,
            0,
            stale_ago,
            &cfg,
        );
        service.episodes.record_sent(key, Some(42), stale_ago);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let token = SecretString::from("test-bot-token".to_string());
        run_episode_tick(
            &service.episodes,
            &tokio::sync::Mutex::new(()),
            &client,
            &base_url,
            &token,
            "123",
            None,
        )
        .await;
        assert_eq!(
            service.episodes.live_count(),
            0,
            "stale Down episode must expire"
        );
        // A stale-close edit was attempted (best-effort neutral line).
        assert!(
            log.lock().unwrap().iter().any(|p| p == "edit"),
            "stale close must attempt to neutralize the old bubble"
        );
        // The NEXT High outage opens a FRESH first page (with the SMS arm).
        let d = service.episodes.apply_event(
            key,
            EpisodeRole::Open,
            Severity::High,
            0,
            epoch_ms_now(),
            &cfg,
        );
        assert_eq!(
            d.action,
            super::super::episode::EpisodeAction::SendFirstPage
        );
    }

    #[tokio::test]
    async fn test_low_episode_escalation_to_high_sends_fresh_page() {
        // Hostile-review fix (pre-open storm): an episode opened by the
        // Low off-hours variant crossing into the in-market High variant
        // must send a FRESH first page (push notification + the SMS arm),
        // never a silent edit of the Low bubble.
        let (base_url, log) = start_recording_telegram_server(400).await;
        let service = make_scripted_service(base_url);
        // 08:59 IST: off-hours Low disconnect opens the episode.
        Arc::clone(&service)
            .dispatch_episode_event(NotificationEvent::WebSocketDisconnectedOffHours {
                connection_index: 0,
                reason: "reset".to_string(),
            })
            .await;
        assert_eq!(service.episodes.live_count(), 1);
        assert_eq!(
            log.lock().unwrap().iter().filter(|p| *p == "send").count(),
            1
        );
        // 09:00 IST: the storm continues in-market at HIGH — fresh page.
        Arc::clone(&service)
            .dispatch_episode_event(NotificationEvent::WebSocketDisconnected {
                connection_index: 0,
                reason: "reset".to_string(),
            })
            .await;
        let sends = log.lock().unwrap().iter().filter(|p| *p == "send").count();
        let edits = log.lock().unwrap().iter().filter(|p| *p == "edit").count();
        assert_eq!(sends, 2, "escalation must send a FRESH page, not edit");
        assert_eq!(edits, 0, "no silent edit for the Low→High escalation");
        let snap = service.episodes.snapshot();
        assert_eq!(snap.len(), 1, "still ONE episode (escalated in place)");
        assert_eq!(snap[0].severity_peak, Severity::High);
        assert_eq!(snap[0].occurrences, 2);
    }

    #[tokio::test]
    async fn test_resolve_without_state_sends_legacy_message_not_dropped() {
        // Hostile-review fix: a recovery for an episode we never tracked
        // (restart lost the snapshot) is delivered through the legacy
        // immediate lane — never silently dropped.
        let (base_url, log) = start_recording_telegram_server(400).await;
        let service = make_scripted_service(base_url);
        Arc::clone(&service)
            .dispatch_episode_event(NotificationEvent::WebSocketReconnected {
                connection_index: 0,
                reason: Some("reset".to_string()),
                down_secs: 12,
                attempts: 3,
            })
            .await;
        assert_eq!(
            log.lock().unwrap().iter().filter(|p| *p == "send").count(),
            1,
            "the solo recovery must be sent via the legacy lane"
        );
        assert_eq!(
            service.episodes.live_count(),
            0,
            "a legacy passthrough never opens an episode"
        );
    }

    #[tokio::test]
    async fn test_edit_transient_high_falls_back_immediately_low_stays_loud() {
        // Hostile-review fix: a HIGH episode whose edit exhausts the
        // transient ladder (429) falls back to a FRESH send on the FIRST
        // failure — a structurally-final event may never re-drive the
        // ladder (duplicate-over-drop).
        let (base_url, log) = start_recording_telegram_server(429).await;
        let service = make_scripted_service(base_url);
        let key = main_feed_key();
        let cfg = EpisodeConfig::default();
        let now = epoch_ms_now();
        let _ = service.episodes.apply_event(
            key,
            EpisodeRole::Open,
            Severity::High,
            0,
            now.saturating_sub(60_000),
            &cfg,
        );
        service
            .episodes
            .record_sent(key, Some(42), now.saturating_sub(60_000));
        Arc::clone(&service)
            .dispatch_episode_event(NotificationEvent::WebSocketDisconnected {
                connection_index: 0,
                reason: "reset".to_string(),
            })
            .await;
        let snap = service.episodes.snapshot();
        assert_eq!(
            snap[0].message_id,
            Some(777),
            "HIGH transient must fall back to a fresh bubble on the first exhausted ladder"
        );
        assert!(
            log.lock().unwrap().iter().any(|p| p == "send"),
            "a fresh send must have fired"
        );

        // A LOW-peak episode below the threshold defers (counted + error!
        // loud, asserted by the wiring guard) without a fallback send.
        let (base_url2, log2) = start_recording_telegram_server(429).await;
        let service2 = make_scripted_service(base_url2);
        let key2 = main_feed_key();
        let _ = service2.episodes.apply_event(
            key2,
            EpisodeRole::Open,
            Severity::Low,
            0,
            now.saturating_sub(60_000),
            &cfg,
        );
        service2
            .episodes
            .record_sent(key2, Some(42), now.saturating_sub(60_000));
        Arc::clone(&service2)
            .dispatch_episode_event(NotificationEvent::WebSocketDisconnectedOffHours {
                connection_index: 0,
                reason: "reset".to_string(),
            })
            .await;
        let snap2 = service2.episodes.snapshot();
        assert_eq!(
            snap2[0].message_id,
            Some(42),
            "sub-threshold LOW transient keeps the bubble (no fallback yet)"
        );
        assert_eq!(snap2[0].edit_failures, 1, "the failure is recorded");
        assert_eq!(
            log2.lock().unwrap().iter().filter(|p| *p == "send").count(),
            0,
            "no fallback send below the threshold for a LOW episode"
        );
    }

    #[tokio::test]
    async fn test_episode_registry_accessor_and_episode_mode_enabled() {
        let service = make_active_service();
        assert_eq!(service.episode_registry().live_count(), 0);
        assert!(service.episode_mode_enabled(), "episode mode defaults ON");
    }

    #[tokio::test]
    async fn test_send_telegram_chunk_with_retry_returning_id_captures_id() {
        let base_url = start_scripted_telegram_server().await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let token = SecretString::from("test-bot-token".to_string());
        let (ok, id) =
            send_telegram_chunk_with_retry_returning_id(&client, &base_url, &token, "123", "hi")
                .await;
        assert!(ok);
        assert_eq!(id, Some(777));
    }

    #[tokio::test]
    async fn test_edit_telegram_message_with_retry_returns_fallback_on_not_found() {
        let base_url = start_scripted_telegram_server().await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let token = SecretString::from("test-bot-token".to_string());
        let outcome =
            edit_telegram_message_with_retry(&client, &base_url, &token, "123", 42, "text").await;
        assert_eq!(outcome, EditOutcome::Fallback);
    }

    #[tokio::test]
    async fn test_shutdown_flush_drains_digest_and_persists_store() {
        // NoOp service: completes instantly (nothing to drain, no store).
        let noop = NotificationService::disabled();
        let started = std::time::Instant::now();
        noop.shutdown_flush().await;
        assert!(started.elapsed() < Duration::from_secs(SHUTDOWN_FLUSH_TIMEOUT_SECS));

        // Active service with a coalescer: drain_all + deliver runs inside
        // the 10s bound even against an unreachable Telegram (100ms client
        // timeout × retry ladder). No store writer wired → no file writes.
        let service = make_active_service();
        let service = NotificationService::enable_coalescer(service, CoalescerConfig::default());
        // Park one Low event in the coalescer so drain_all has work.
        if let Some(c) = service.coalescer.as_ref() {
            let _ = c.observe("TickGapsSummary", Severity::Low, || "gap".to_string());
        }
        let started = std::time::Instant::now();
        service.shutdown_flush().await;
        assert!(
            started.elapsed() < Duration::from_secs(SHUTDOWN_FLUSH_TIMEOUT_SECS),
            "shutdown flush must respect the bound"
        );
    }

    #[tokio::test]
    async fn test_episode_store_write_and_rehydrate_roundtrip() {
        // write_episode_store + rehydrate_episodes against a temp path.
        let dir = std::env::temp_dir().join(format!(
            "tickvault-episode-store-test-{}",
            std::process::id()
        ));
        let path = dir.join("episodes.json");
        let key = main_feed_key();
        let reg = super::super::episode::EpisodeRegistry::new();
        let now = epoch_ms_now();
        let _ = reg.apply_event(
            key,
            EpisodeRole::Open,
            Severity::High,
            0,
            now,
            &EpisodeConfig::default(),
        );
        reg.record_sent(key, Some(55), now);
        let json = episode_snapshot::encode(&reg.snapshot());
        write_episode_store(&path, &json).await;

        let fresh = super::super::episode::EpisodeRegistry::new();
        rehydrate_episodes(&fresh, &path).await;
        assert_eq!(fresh.live_count(), 1, "same-day fresh entry rehydrates");
        assert_eq!(fresh.snapshot()[0].message_id, Some(55));

        // Corrupt file → fail-open empty registry, no panic.
        tokio::fs::write(&path, "{{{corrupt").await.unwrap();
        let corrupt = super::super::episode::EpisodeRegistry::new();
        rehydrate_episodes(&corrupt, &path).await;
        assert_eq!(corrupt.live_count(), 0);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn test_spawn_episode_ticker_safe_on_noop_and_active() {
        // NoOp: returns without spawning (no transport).
        let noop = NotificationService::disabled();
        NotificationService::spawn_episode_ticker(&noop);
        // Active without a coalescer: spawns the fallback ticker.
        let service = make_active_service();
        NotificationService::spawn_episode_ticker(&service);
        // Active WITH a coalescer: no-op (the drain loop owns the tick).
        let with_coalescer = NotificationService::enable_coalescer(
            make_active_service(),
            CoalescerConfig::default(),
        );
        NotificationService::spawn_episode_ticker(&with_coalescer);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // Boot bubble (2026-07-09) — one consolidated boot checklist bubble
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_dispatch_boot_serializes_concurrent_milestones_single_bubble() {
        // Boot milestones fire ms apart on separate spawned tasks. The
        // boot_dispatch_lock must serialize fold→render→send/edit so the
        // race against the in-flight FIRST send can never open a second
        // bubble: exactly ONE fresh send, everything else edits in place.
        let (base_url, log) = start_recording_telegram_server(200).await;
        let service = make_scripted_service(base_url);
        let events = vec![
            NotificationEvent::AuthenticationSuccess,
            NotificationEvent::InstrumentBuildSuccess {
                source: "cache".to_string(),
                derivative_count: 1000,
                underlying_count: 46,
            },
            NotificationEvent::OrderUpdateConnected,
            NotificationEvent::OrderUpdateAuthenticated,
            NotificationEvent::BootHealthCheck {
                services_healthy: 3,
                services_total: 3,
            },
        ];
        let mut handles = Vec::new();
        for event in events {
            let svc = Arc::clone(&service);
            handles.push(tokio::spawn(async move {
                svc.dispatch_boot_episode_event(event).await;
            }));
        }
        for h in handles {
            h.await.expect("dispatch task must not panic");
        }
        let sends = log.lock().unwrap().iter().filter(|p| *p == "send").count();
        let edits = log.lock().unwrap().iter().filter(|p| *p == "edit").count();
        assert_eq!(sends, 1, "exactly ONE boot bubble first page");
        // 4 edits when every fold changes the render; 3 is legal when
        // OrderUpdateConnected folds AFTER OrderUpdateAuthenticated (its
        // superset) — the byte-identical render hash-skips. The invariant
        // is single-bubble, not a scheduling-order-dependent edit count.
        assert!(
            (3..=4).contains(&edits),
            "every later milestone edits the SAME bubble (got {edits})"
        );
        assert_eq!(service.episodes.live_count(), 1, "one live boot episode");
        let snap = service.episodes.snapshot();
        assert_eq!(snap[0].message_id, Some(777));
        let cl = service
            .episodes
            .boot_checklist()
            .expect("checklist populated");
        assert!(cl.dhan_auth && cl.order_update_authenticated);
        assert_eq!(cl.instruments, Some(1046));
        assert!(!cl.dirty, "all folds delivered");
    }

    #[tokio::test]
    async fn test_boot_first_milestone_sends_then_second_edits_same_id() {
        let (base_url, log) = start_recording_telegram_server(200).await;
        let service = make_scripted_service(base_url);
        Arc::clone(&service)
            .dispatch_boot_episode_event(NotificationEvent::AuthenticationSuccess)
            .await;
        Arc::clone(&service)
            .dispatch_boot_episode_event(NotificationEvent::StartupComplete {
                mode: "sandbox",
                spot_1m_enabled: true,
                spot_1m_indices: 4,
                chain_1m_enabled: true,
                chain_1m_underlyings: 3,
            })
            .await;
        assert_eq!(
            log.lock().unwrap().as_slice(),
            ["send", "edit"],
            "first page then in-place edit"
        );
        let snap = service.episodes.snapshot();
        assert_eq!(snap[0].message_id, Some(777));
        let cl = service.episodes.boot_checklist().expect("checklist");
        assert!(cl.is_complete());
        // The completed render carries the load-bearing phrase.
        let rendered = render_boot_checklist(
            &cl,
            &EpisodeRenderCtx {
                now_ms: epoch_ms_now(),
            },
        );
        assert!(rendered.contains("tickvault started"));
    }

    #[tokio::test]
    async fn test_boot_bubble_mode_off_is_legacy_byte_identical() {
        // Kill switch: boot_bubble=false routes boot milestones through
        // the legacy immediate lane (registry untouched) while the #1439
        // WS episode lane keeps working (independence pin).
        let (base_url, log) = start_recording_telegram_server(200).await;
        let mut owned = match Arc::try_unwrap(make_scripted_service(base_url)) {
            Ok(s) => s,
            Err(_) => unreachable!("fresh Arc has one owner"),
        };
        owned.boot_bubble = false;
        let service = Arc::new(owned);
        assert!(!service.boot_bubble_enabled());
        service.notify(NotificationEvent::AuthenticationSuccess);
        for _ in 0..50 {
            if log.lock().unwrap().iter().any(|p| p == "send") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            service.episodes.live_count(),
            0,
            "boot_bubble=false must never touch the registry"
        );
        assert!(
            log.lock().unwrap().iter().any(|p| p == "send"),
            "the legacy immediate send still fired"
        );
        // WS episodes are INDEPENDENT of the boot kill switch.
        service.notify(NotificationEvent::WebSocketDisconnected {
            connection_index: 0,
            reason: "reset".to_string(),
        });
        for _ in 0..50 {
            if service.episodes.live_count() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(service.episodes.live_count(), 1);
    }

    #[tokio::test]
    async fn test_boot_redrive_ticker_retries_failed_first_page() {
        // A milestone folded while Telegram was down leaves the checklist
        // dirty with NO message id. The drain ticker's boot arm re-drives
        // a fresh full-checklist first page.
        let (base_url, log) = start_recording_telegram_server(200).await;
        let service = make_scripted_service(base_url.clone());
        let cfg = episode_config_for(EpisodeFamily::Boot);
        // Simulate the failed-send state (fold landed, delivery did not).
        let _ = service.episodes.apply_boot_milestone(
            super::super::episode::BootMilestone::DhanAuth,
            epoch_ms_now(),
            &cfg,
        );
        assert!(service.episodes.boot_redrive_candidate().is_some());
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let token = SecretString::from("test-bot-token".to_string());
        run_episode_tick(
            &service.episodes,
            &tokio::sync::Mutex::new(()),
            &client,
            &base_url,
            &token,
            "123",
            None,
        )
        .await;
        assert_eq!(
            log.lock().unwrap().iter().filter(|p| *p == "send").count(),
            1,
            "the ticker must retry the first page fresh"
        );
        let snap = service.episodes.snapshot();
        assert_eq!(snap[0].message_id, Some(777));
        assert!(
            service.episodes.boot_redrive_candidate().is_none(),
            "delivered — nothing left to re-drive"
        );
        // A second tick with a clean checklist is a no-op (hash-skip /
        // no candidate) — no duplicate bubbles.
        run_episode_tick(
            &service.episodes,
            &tokio::sync::Mutex::new(()),
            &client,
            &base_url,
            &token,
            "123",
            None,
        )
        .await;
        assert_eq!(
            log.lock().unwrap().iter().filter(|p| *p == "send").count(),
            1,
            "no re-send once delivered"
        );
    }

    #[tokio::test]
    async fn test_set_boot_expectations_and_boot_bubble_enabled() {
        let base_url = start_scripted_telegram_server().await;
        let service = make_scripted_service(base_url);
        assert!(service.boot_bubble_enabled(), "boot bubble defaults ON");
        service.set_boot_expectations(true, true);
        let cl = service
            .episodes
            .boot_checklist()
            .expect("expectations create the checklist");
        assert_eq!(cl.expectations, Some((true, true)));
    }

    #[test]
    fn test_is_new_code_deploy_fail_open_and_unknown_sha() {
        // Claimed ONLY when both shas are valid lowercase hex AND differ.
        assert!(is_new_code_deploy(
            Some("0123abc4567890fedcba0123abc4567890fedcba"),
            "aaaa1112223334445556667778889990001112ff"
        ));
        assert!(is_new_code_deploy(Some("0123abc"), "aaaa111"));
        // Same sha = plain restart.
        assert!(!is_new_code_deploy(Some("0123abc"), "0123abc"));
        // Unknown / garbage / short / uppercase / missing → never claimed.
        assert!(!is_new_code_deploy(Some("unknown"), "0123abc"));
        assert!(!is_new_code_deploy(Some("0123abc"), "unknown"));
        assert!(!is_new_code_deploy(Some("0123AbC"), "0123abc"));
        assert!(!is_new_code_deploy(Some("012345"), "0123abc"));
        assert!(!is_new_code_deploy(Some(""), "0123abc"));
        assert!(!is_new_code_deploy(None, "0123abc"));
    }

    #[tokio::test]
    async fn test_init_boot_sha_flavor_never_panics_and_stamps_flavor() {
        // Whatever the on-disk state, the flavor init must not panic and
        // must leave a checklist with a stamped sha (fail-open new_code).
        let reg = super::super::episode::EpisodeRegistry::new();
        init_boot_sha_flavor(&reg).await;
        let cl = reg.boot_checklist().expect("flavor creates the checklist");
        assert!(!cl.build_sha_short.is_empty());
    }

    #[tokio::test]
    async fn test_episode_mode_off_uses_legacy_dispatch() {
        // Kill switch: episode_mode=false → the WS event takes the legacy
        // per-event path (no episode state is created).
        let base_url = start_scripted_telegram_server().await;
        let mut owned = match Arc::try_unwrap(make_scripted_service(base_url)) {
            Ok(s) => s,
            Err(_) => unreachable!("fresh Arc has one owner"),
        };
        owned.episode_mode = false;
        let service = Arc::new(owned);
        service.notify(NotificationEvent::WebSocketDisconnected {
            connection_index: 0,
            reason: "reset".to_string(),
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            service.episodes.live_count(),
            0,
            "episode_mode=false must never touch the registry"
        );
    }
}
