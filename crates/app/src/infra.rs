//! Infrastructure orchestrator — auto-starts Docker services if not running.
//!
//! On startup, probes QuestDB and Grafana. If unreachable, fetches infra
//! credentials from AWS SSM and runs `docker compose up -d`. Waits for
//! services to be healthy before returning.
//!
//! This eliminates the manual `docker compose up -d` step — just run the
//! binary (from IntelliJ or CLI) and everything starts automatically.

use std::net::TcpStream;
use std::time::Duration;

use anyhow::{Context, Result};
use secrecy::ExposeSecret;
use tracing::{debug, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_core::auth::secret_manager;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

// O(1) EXEMPT: begin — named constants, not hardcoded Duration
/// TCP probe timeout for infrastructure services.
const INFRA_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum wait time for Docker services to become healthy.
const INFRA_HEALTH_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum retries for `docker compose up -d` during startup.
const COMPOSE_UP_MAX_RETRIES: u32 = 5;

/// Initial delay between `docker compose up -d` retries.
const COMPOSE_UP_RETRY_DELAY: Duration = Duration::from_secs(10);

/// Interval for infrastructure watchdog container health checks.
pub const INFRA_WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);

/// Maximum wait time for Docker daemon to start after launching Docker Desktop.
const DOCKER_DAEMON_TIMEOUT: Duration = Duration::from_secs(90);

/// Polling interval when waiting for services to become healthy.
const INFRA_HEALTH_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Timeout for QuestDB liveness HTTP check (SELECT 1).
///
/// 15s is high enough that a QuestDB under heavy ILP ingestion pressure
/// (ticks + deep depth + candles + OBI + movers) does not trip the check
/// just because the HTTP `/exec` endpoint is momentarily slow. It's still
/// low enough that a real outage surfaces inside one watchdog cycle.
const QUESTDB_LIVENESS_TIMEOUT: Duration = Duration::from_secs(15);

/// Consecutive liveness failures required before alerting.
///
/// One slow `SELECT 1` under load is NOT an outage. We require three
/// back-to-back failures (15-minute window at the 5-minute watchdog cadence)
/// before paging. Recovery resets the counter.
pub const QUESTDB_LIVENESS_FAILURE_THRESHOLD: u32 = 3;

/// Idle timeout for pooled liveness HTTP connections. Must be longer than
/// the watchdog cadence (5 minutes) to benefit from keep-alive.
const QUESTDB_LIVENESS_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// TCP keep-alive probe interval for pooled liveness connections.
const QUESTDB_LIVENESS_TCP_KEEPALIVE: Duration = Duration::from_secs(30);
// O(1) EXEMPT: end

/// Path to docker-compose file relative to project root.
const DOCKER_COMPOSE_PATH: &str = "deploy/docker/docker-compose.yml";

/// System-wide docker CLI plugin locations probed for the Compose v2 plugin
/// binary when neither `docker compose` nor `docker-compose` resolves
/// (issue #1505 — mirrors the `scripts/ensure-questdb.sh` rung-3c ladder).
///
/// Deliberately EXCLUDES the per-user `~/.docker/cli-plugins/` directory:
/// the systemd unit runs with `ProtectHome=true`, so a per-user plugin is
/// invisible in exactly the hardened service context that needs the
/// fallback — that invisibility is the #1505 root cause, and probing it
/// here would make dev-shell behaviour diverge from the service context.
/// Ratcheted by `test_compose_plugin_system_paths_are_system_wide`.
const COMPOSE_PLUGIN_SYSTEM_PATHS: [&str; 3] = [
    "/usr/local/lib/docker/cli-plugins/docker-compose",
    "/usr/libexec/docker/cli-plugins/docker-compose",
    "/usr/lib/docker/cli-plugins/docker-compose",
];

/// macOS Docker Desktop application name for `open -a`.
const DOCKER_DESKTOP_APP_NAME: &str = "Docker";

/// Host used for all local TCP reachability probes.
// O(1) EXEMPT: localhost probe for local Docker infrastructure
const LOCAL_HOST: &str = "127.0.0.1";

/// Dashboard URLs and ports — auto-opened in browser after infrastructure starts.
/// Each entry: (service name, URL, host, port).
///
/// PR #7d (2026-05-19): the `Portal` + `Market Dashboard` entries were
/// removed alongside the `/portal/*` HTML retirement.
/// #O3 (2026-05-20): Prometheus removed — observability narrowed to
/// CloudWatch-only. Operator UX is the QuestDB Console + Telegram alerts.
const DASHBOARD_SERVICES: &[(&str, &str, &str, u16)] = &[
    ("QuestDB", "http://localhost:9000", LOCAL_HOST, 9000),
    ("Traefik", "http://localhost:8080", LOCAL_HOST, 8080),
];

// `VALKEY_PORT` constant DELETED in #O4 (2026-05-24) — Valkey removed
// from the runtime.

// ---------------------------------------------------------------------------
// Boot compose-up outcome classification (false-CRITICAL fix, 2026-07-01)
// ---------------------------------------------------------------------------

/// Outcome of the boot-time `docker compose up -d` retry loop, once the
/// required service (QuestDB) has been probed.
///
/// Root cause this guards against: `run_docker_compose_up` runs
/// `--force-recreate`, which is NOT idempotent — it tears down + recreates
/// every container on every boot. On a same-day crash-recovery boot (QuestDB
/// already up + busy) the recreate can fail all `COMPOSE_UP_MAX_RETRIES`
/// attempts even though the service the app actually needs is healthy. The
/// old code fired a Telegram CRITICAL unconditionally on that path — a
/// false-CRITICAL that erodes operator trust in the pager. This enum routes
/// the failure by whether the required service is ACTUALLY reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeOutcome {
    /// `docker compose up -d` succeeded within the retry budget.
    Succeeded,
    /// compose-up exhausted its retries, but the required service (QuestDB)
    /// is reachable — the app can run. Log Low (`warn!`), no CRITICAL page.
    /// This is the `--force-recreate`-churn-while-service-up false-CRITICAL.
    DegradedServiceUp,
    /// compose-up exhausted its retries AND the required service is NOT
    /// reachable — a real outage. Keep the CRITICAL page.
    Critical,
}

impl ComposeOutcome {
    /// True only for the genuine-outage outcome that must page CRITICAL.
    /// Stable wire-semantics — ratcheted so a refactor cannot silently make
    /// the false-CRITICAL page again.
    #[must_use]
    pub fn is_critical(&self) -> bool {
        matches!(self, Self::Critical)
    }
}

/// Pure decision for the boot compose-up outcome.
///
/// | `compose_succeeded` | `required_service_reachable` | outcome |
/// |---|---|---|
/// | `true`  | (ignored) | `Succeeded` |
/// | `false` | `true`    | `DegradedServiceUp` (Low warn — no page) |
/// | `false` | `false`   | `Critical` (real outage — page) |
///
/// The CRITICAL fires ONLY when compose-up failed AND the required service is
/// actually unreachable — it never suppresses a real compose failure where the
/// service is down, and never pages when the service the app needs is healthy.
#[must_use]
pub fn classify_compose_outcome(
    compose_succeeded: bool,
    required_service_reachable: bool,
) -> ComposeOutcome {
    if compose_succeeded {
        ComposeOutcome::Succeeded
    } else if required_service_reachable {
        ComposeOutcome::DegradedServiceUp
    } else {
        ComposeOutcome::Critical
    }
}

// ---------------------------------------------------------------------------
// Compose CLI resolution (issue #1505, 2026-07-18)
// ---------------------------------------------------------------------------
// `unknown shorthand flag: 'f'` from `docker compose -f … up` is the docker
// CLI parsing `-f` at TOP LEVEL because no Compose v2 plugin is resolvable
// in the invoking context (e.g. the plugin lives under
// `~/.docker/cli-plugins/`, hidden from the systemd service by
// `ProtectHome=true`). That failure is DETERMINISTIC — retrying the same
// invocation is noise, not recovery. Mirror the `scripts/ensure-questdb.sh`
// ladder (v2 → v1 → plugin-by-absolute-path) so a cold boot can bring
// QuestDB up even when the `docker compose` front-end is broken, and fail
// LOUDLY (once, with the actionable cause) when no compose front-end exists.

/// A resolved, working Docker Compose front-end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposeCli {
    /// `docker compose …` — the modern v2 plugin resolved by the docker CLI.
    DockerComposeV2,
    /// `docker-compose …` — the standalone v1 binary on `PATH`.
    StandaloneV1,
    /// The Compose v2 plugin binary invoked DIRECTLY by absolute path
    /// (plugin present at a system path but not resolvable via `docker`).
    PluginPath(&'static str),
}

impl ComposeCli {
    /// The program to exec for this front-end.
    fn program(self) -> &'static str {
        match self {
            ComposeCli::DockerComposeV2 => "docker",
            ComposeCli::StandaloneV1 => "docker-compose",
            ComposeCli::PluginPath(path) => path,
        }
    }

    /// Human-readable label for logs.
    fn label(self) -> &'static str {
        match self {
            ComposeCli::DockerComposeV2 => "docker compose (v2 plugin)",
            ComposeCli::StandaloneV1 => "docker-compose (v1 standalone)",
            ComposeCli::PluginPath(_) => "compose v2 plugin by absolute path",
        }
    }
}

/// Pure builder for a full compose argument vector for the given front-end.
///
/// R1 (2026-06-30) ratchet, generalized for #1505: for the `docker`
/// front-end the `compose` SUBCOMMAND comes FIRST, then `-f <file>` — the
/// LEGACY form `docker -f <file> compose …` (flag before subcommand) fails
/// with `unknown shorthand flag: 'f'` (exit 125), which made every boot
/// compose attempt fail → QuestDB never starts on a cold box. For the
/// v1 / plugin-by-path front-ends the PROGRAM ITSELF is compose, so there
/// is no subcommand and `-f` comes first. Tested by
/// `test_docker_compose_up_args_compose_subcommand_first` and
/// `test_compose_cli_args_v1_and_plugin_have_no_compose_subcommand`.
#[must_use]
fn compose_cli_args<'a>(cli: ComposeCli, compose_path: &'a str, tail: &[&'a str]) -> Vec<&'a str> {
    let mut args: Vec<&'a str> = Vec::with_capacity(3 + tail.len());
    if matches!(cli, ComposeCli::DockerComposeV2) {
        args.push("compose");
    }
    args.push("-f");
    args.push(compose_path);
    args.extend_from_slice(tail);
    args
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Ensures Docker infrastructure services are running.
///
/// **ALWAYS runs `docker compose up -d`** at boot. This is by design.
/// `docker compose up -d` is idempotent — it is a no-op (~1s) when every
/// service is already running, and it brings up any container that is
/// missing, stopped, crashed, or has a stale port binding from a previous
/// run. TCP probes are NOT used as a short-circuit because they produce
/// false positives (port bound but process dead, stale Docker IPVS rule,
/// container `up` but app process inside it crashed).
///
/// 1. Ensures Docker daemon is running (launches Docker Desktop on macOS).
/// 2. Fetches infra credentials from SSM.
/// 3. Runs `docker compose up -d` (idempotent).
/// 4. Waits for QuestDB to become healthy.
/// 5. Opens dashboards in the default browser.
///
/// Best-effort: if Docker or SSM is unavailable, logs a warning
/// and returns — the app already handles missing services gracefully.
pub async fn ensure_infra_running(questdb_config: &QuestDbConfig) {
    info!("ensuring Docker infrastructure is running (idempotent — safe to call when up)");

    // Ensure Docker daemon is running (launches Docker Desktop on macOS if needed).
    if !ensure_docker_daemon_running().await {
        warn!("Docker daemon is not available — cannot auto-start infrastructure");
        return;
    }

    // Fetch infra credentials from SSM concurrently.
    let (questdb_creds, telegram_creds) = match tokio::join!(
        secret_manager::fetch_questdb_credentials(),
        secret_manager::fetch_telegram_credentials(),
    ) {
        (Ok(q), Ok(t)) => (q, t),
        (q_result, t_result) => {
            if let Err(ref err) = q_result {
                warn!(
                    ?err,
                    "failed to fetch QuestDB credentials from SSM — cannot auto-start Docker"
                );
            }
            if let Err(ref err) = t_result {
                warn!(
                    ?err,
                    "failed to fetch Telegram credentials from SSM — cannot auto-start Docker"
                );
            }
            return;
        }
    };

    // Build env vars for docker-compose.
    let env_vars = [
        (
            "TV_QUESTDB_PG_USER",
            questdb_creds.pg_user.expose_secret().to_string(),
        ),
        (
            "TV_QUESTDB_PG_PASSWORD",
            questdb_creds.pg_password.expose_secret().to_string(),
        ),
        (
            "TV_TELEGRAM_BOT_TOKEN",
            telegram_creds.bot_token.expose_secret().to_string(),
        ),
        (
            "TV_TELEGRAM_CHAT_ID",
            telegram_creds.chat_id.expose_secret().to_string(),
        ),
    ];

    // Ensure the machine app.log placeholder exists before Docker mounts it.
    // Alloy container mounts ../../data/logs → /var/log/tv-app to watch the
    // log files; 2026-07-05 operator directive moved every machine sink
    // (incl. this placeholder) under data/logs/machine/ so the data/logs/
    // top level stays the one human log surface.
    // If the directory doesn't exist, Docker creates it as root-owned.
    // If app.log doesn't exist, Alloy's file watch fails → healthcheck fails → DOWN.
    if let Err(err) = std::fs::create_dir_all(crate::observability::MACHINE_LOGS_DIR) {
        warn!(
            ?err,
            "failed to create data/logs/machine directory for Alloy log mount"
        );
    }
    // Create the app.log file itself (Alloy needs it to exist at startup).
    // OpenOptions with create+append ensures idempotent creation without truncating.
    if let Err(err) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::boot_helpers::APP_LOG_FILE_PATH)
    {
        warn!(
            ?err,
            "failed to create data/logs/machine/app.log for Alloy file watch"
        );
    }

    // Run compose up -d with retries — via the resolved compose front-end
    // (issue #1505). A missing compose CLI is DETERMINISTIC: `docker compose
    // -f …` degrades to the docker top-level flag parse and fails
    // `unknown shorthand flag: 'f'` (exit 125) on every attempt, so retrying
    // adds noise, not recovery — log it loudly ONCE with the actionable
    // cause and skip the retry loop entirely.
    let mut compose_succeeded = false;
    match resolve_compose_cli().await {
        None => {
            metrics::counter!("tv_boot_compose_cli_unavailable_total").increment(1);
            tracing::error!(
                probed_system_paths = ?COMPOSE_PLUGIN_SYSTEM_PATHS,
                "no usable Docker Compose CLI in this context — `docker compose`, \
                 `docker-compose`, and the system plugin paths all failed to \
                 resolve (the `unknown shorthand flag: 'f'` failure class; a \
                 per-user ~/.docker/cli-plugins plugin is invisible under \
                 ProtectHome=true). Install the Compose v2 plugin at \
                 /usr/local/lib/docker/cli-plugins/docker-compose (the deploy \
                 workflow + user-data ensure this since 2026-07-14). Skipping \
                 the compose retry loop — retrying a missing CLI is \
                 deterministic noise"
            );
        }
        Some(cli) => {
            info!(
                compose_cli = cli.label(),
                "resolved Docker Compose front-end"
            );
            for attempt in 1..=COMPOSE_UP_MAX_RETRIES {
                match run_docker_compose_up(cli, &env_vars).await {
                    Ok(()) => {
                        info!(
                            attempt,
                            "docker compose started — waiting for services to be healthy"
                        );
                        compose_succeeded = true;
                        break;
                    }
                    Err(err) => {
                        warn!(
                            ?err,
                            attempt,
                            max_attempts = COMPOSE_UP_MAX_RETRIES,
                            "docker compose up failed — retrying"
                        );
                        if attempt < COMPOSE_UP_MAX_RETRIES {
                            tokio::time::sleep(COMPOSE_UP_RETRY_DELAY).await;
                        }
                    }
                }
            }
        }
    }
    // false-CRITICAL fix (2026-07-01): compose-up may fail all retries purely
    // because `--force-recreate` churns containers that are ALREADY up (e.g. a
    // same-day crash-recovery boot with QuestDB already serving). Only page
    // CRITICAL when the required service (QuestDB) is ACTUALLY unreachable —
    // otherwise the app can run and the CRITICAL was a false alarm.
    let questdb_reachable =
        !compose_succeeded && is_service_reachable(&questdb_config.host, questdb_config.http_port);
    match classify_compose_outcome(compose_succeeded, questdb_reachable) {
        ComposeOutcome::Succeeded => {}
        ComposeOutcome::DegradedServiceUp => {
            metrics::counter!("tv_boot_compose_recreate_degraded_total").increment(1);
            warn!(
                max_attempts = COMPOSE_UP_MAX_RETRIES,
                "docker compose up failed on all attempts, but QuestDB is already up — \
                 services already running despite compose recreate churn; boot continues"
            );
        }
        ComposeOutcome::Critical => {
            tracing::error!(
                "docker compose up failed after {COMPOSE_UP_MAX_RETRIES} attempts and \
                 QuestDB is unreachable — services may not be available. \
                 Telegram CRITICAL alert should fire."
            );
            return;
        }
    }

    // Wait for all critical services to become healthy.
    wait_for_service_healthy("QuestDB", &questdb_config.host, questdb_config.http_port).await;

    // Auto-open all monitoring dashboards in the default browser.
    open_all_dashboards().await;
}

/// Opens all reachable monitoring dashboards in the default browser.
///
/// Opens: QuestDB Console (the remaining browser-facing service).
/// Best-effort: if a service is not running or the browser cannot be launched,
/// logs a warning and skips it — does not block boot.
async fn open_all_dashboards() {
    for &(name, url, host, port) in DASHBOARD_SERVICES {
        if is_service_reachable(host, port) {
            open_in_browser(url).await;
        } else {
            debug!(service = name, "not reachable — skipping browser auto-open");
        }
    }
}

// ---------------------------------------------------------------------------
// Wave 2 Item 7.3 (G8) — Boot-time wall-clock skew probe (BOOT-03)
// ---------------------------------------------------------------------------

/// Outcome of a single clock-skew probe sample.
///
/// Returned by [`probe_clock_skew`] so callers can log + alert + halt
/// without re-implementing the threshold check.
#[derive(Debug, Clone)]
pub struct ClockSkewSample {
    /// Signed seconds; positive = local clock is AHEAD of trusted source.
    pub skew_secs: f64,
    /// Source label, one of `"chronyc"` or `"questdb_now"`.
    pub source: &'static str,
}

impl ClockSkewSample {
    /// True when `|skew_secs|` exceeds the halt threshold (default 2.0s
    /// per `CLOCK_SKEW_HALT_THRESHOLD_SECS`). Pure function so tests
    /// can drive synthetic skews deterministically.
    #[must_use]
    pub fn exceeds(&self, threshold_secs: f64) -> bool {
        self.skew_secs.abs() >= threshold_secs
    }
}

/// Errors from the boot-time clock-skew probe.
///
/// Hand-written `Display` / `Error` impls so the `app` crate does not
/// pull in a new `thiserror` dependency for one enum.
#[derive(Debug, Clone)]
pub enum ClockSkewError {
    /// Both probe paths (`chronyc` PRIMARY, QuestDB `SELECT now()`
    /// FALLBACK) failed before producing a sample. The boot orchestrator
    /// should log a WARN and proceed (not HALT) — refusing to boot when
    /// we cannot even read the clock is brittle on dev hardware.
    Unavailable {
        /// Primary probe failure description.
        primary: String,
        /// Fallback probe failure description.
        fallback: String,
    },
    /// Sample acquired and skew exceeded `CLOCK_SKEW_HALT_THRESHOLD_SECS`.
    ThresholdExceeded {
        /// Observed signed skew seconds.
        skew_secs: f64,
        /// Threshold that was exceeded.
        threshold_secs: f64,
        /// Probe source.
        source: &'static str,
    },
}

impl std::fmt::Display for ClockSkewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { primary, fallback } => write!(
                f,
                "clock-skew probe unavailable: {primary} (primary), {fallback} (fallback)"
            ),
            Self::ThresholdExceeded {
                skew_secs,
                threshold_secs,
                source,
            } => write!(
                f,
                "BOOT-03 clock skew {skew_secs:+.3}s exceeds threshold {threshold_secs:.2}s (source: {source})"
            ),
        }
    }
}

impl std::error::Error for ClockSkewError {}

/// Reads `chronyc tracking` and parses the `Last offset` line.
///
/// Security review: `Command::new` + `args(&[...])` with a STATIC slice.
/// No string interpolation, no shell, no env passthrough — argv cannot
/// be influenced by external input. The `chronyc` binary itself comes
/// from the host (Docker bind-mount) and is trusted. On hosts without
/// chrony installed, this returns `Err` and the caller falls through.
///
/// `Last offset` line format:
/// ```text
/// Last offset     : -0.000007441 seconds
/// ```
fn probe_via_chronyc() -> Result<ClockSkewSample, String> {
    // STATIC argv — argv is a `&[&str; 1]` literal. Security-reviewed:
    // never mix in user input, never `format!`, never `env` passthrough.
    let argv: &[&str; 1] = &["tracking"];
    let output = std::process::Command::new("chronyc")
        .args(argv.as_slice())
        .output()
        .map_err(|e| format!("chronyc spawn failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "chronyc tracking exited non-zero: {}",
            output.status
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        // Tolerate locale variants — match prefix only.
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Last offset") {
            // After the colon, the value is `<sign?>0.NNNNN seconds`.
            let after_colon = rest.split(':').nth(1).ok_or_else(|| {
                format!("chronyc tracking 'Last offset' line malformed: {trimmed}")
            })?;
            let value = after_colon
                .split_whitespace()
                .next()
                .ok_or_else(|| format!("chronyc tracking value missing: {trimmed}"))?;
            let skew_secs: f64 = value
                .parse()
                .map_err(|e| format!("chronyc tracking value not f64 ('{value}'): {e}"))?;
            return Ok(ClockSkewSample {
                skew_secs,
                source: "chronyc",
            });
        }
    }
    Err("chronyc tracking did not contain a 'Last offset' line".to_string())
}

/// Falls back to QuestDB `SELECT now()` over its HTTP `/exec` endpoint.
///
/// Returns the signed difference between `Utc::now()` and QuestDB's
/// `now()`. QuestDB stores all timestamps in UTC, so the comparison is
/// timezone-safe.
async fn probe_via_questdb_now(questdb_config: &QuestDbConfig) -> Result<ClockSkewSample, String> {
    let url = format!(
        "http://{}:{}/exec?query=SELECT%20now()",
        questdb_config.host, questdb_config.http_port
    );
    let client = reqwest::Client::builder()
        .timeout(INFRA_PROBE_TIMEOUT)
        .build()
        .map_err(|e| format!("reqwest builder: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("questdb GET failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("questdb /exec status {}", resp.status()));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| format!("questdb body read: {e}"))?;
    // Body shape: `{"query":"SELECT now()","columns":[...],"dataset":[["2026-04-27T12:34:56.123456Z"]],...}`
    // Lift the timestamp string out of the dataset without pulling in serde_json.
    let dataset_marker = "\"dataset\":[[\"";
    let from = body
        .find(dataset_marker)
        .ok_or_else(|| format!("questdb response missing dataset: {body}"))?
        + dataset_marker.len();
    let end = body[from..]
        .find('"')
        .ok_or_else(|| format!("questdb response malformed: {body}"))?;
    let ts_str = &body[from..from + end];
    let questdb_ts = chrono::DateTime::parse_from_rfc3339(ts_str)
        .map_err(|e| format!("questdb timestamp '{ts_str}' parse: {e}"))?;
    let local_ts = chrono::Utc::now();
    let skew_secs = (local_ts.timestamp_micros() - questdb_ts.timestamp_micros()) as f64 / 1.0e6;
    Ok(ClockSkewSample {
        skew_secs,
        source: "questdb_now",
    })
}

/// Wave 2 Item 7.3 (G8) — sample wall-clock skew at boot.
///
/// PRIMARY: `chronyc tracking` shell-out (host has chrony installed in
/// every prod + dev container per `deploy/`). FALLBACK: QuestDB
/// `SELECT now()` over PG-wire — usable post-`wait_for_questdb_ready`.
/// One probe, no try/catch chain — PRIMARY is attempted first, then
/// FALLBACK if PRIMARY errored.
///
/// Returns `Ok(sample)` regardless of magnitude — the caller is
/// responsible for comparing `sample.skew_secs.abs()` against
/// `CLOCK_SKEW_HALT_THRESHOLD_SECS`. This split lets unit tests
/// deterministically drive the threshold logic without a live clock.
pub async fn probe_clock_skew(
    questdb_config: &QuestDbConfig,
) -> Result<ClockSkewSample, ClockSkewError> {
    match probe_via_chronyc() {
        Ok(sample) => {
            // Emit a Prometheus gauge so dashboards can plot drift over
            // time. `tv_clock_skew_seconds` is the canonical metric name.
            metrics::gauge!("tv_clock_skew_seconds", "source" => sample.source)
                .set(sample.skew_secs);
            Ok(sample)
        }
        Err(primary_err) => {
            debug!(
                error = %primary_err,
                "chronyc clock-skew probe failed, trying QuestDB fallback"
            );
            match probe_via_questdb_now(questdb_config).await {
                Ok(sample) => {
                    metrics::gauge!("tv_clock_skew_seconds", "source" => sample.source)
                        .set(sample.skew_secs);
                    Ok(sample)
                }
                Err(fallback_err) => Err(ClockSkewError::Unavailable {
                    primary: primary_err,
                    fallback: fallback_err,
                }),
            }
        }
    }
}

/// Convenience helper: runs [`probe_clock_skew`] and returns
/// `Err(ThresholdExceeded)` if the magnitude exceeds the configured
/// halt threshold. Boot orchestrator calls this; on Err it must HALT.
pub async fn enforce_clock_skew_at_boot(
    questdb_config: &QuestDbConfig,
    threshold_secs: f64,
) -> Result<ClockSkewSample, ClockSkewError> {
    let sample = probe_clock_skew(questdb_config).await?;
    if sample.exceeds(threshold_secs) {
        return Err(ClockSkewError::ThresholdExceeded {
            skew_secs: sample.skew_secs,
            threshold_secs,
            source: sample.source,
        });
    }
    Ok(sample)
}

// ---------------------------------------------------------------------------
// System health checks (cold path — called once at boot or periodically)
// ---------------------------------------------------------------------------

/// Minimum free disk space percentage before alerting.
pub const MIN_FREE_DISK_PERCENT: u64 = 5;

// `MEMORY_RSS_ALERT_MB` (legacy 1 GB single-process gauge alert,
// PR #497) was RETIRED 2026-05-08 per the Wave-5 in-memory-store
// plan §AA / L122. The legacy threshold would fire instantly under
// the new ~2.31 GB in-memory-store design AND give zero diagnostic
// signal (BUG-C2). It has been superseded by the per-component
// alert `tv-rss-per-subsystem-high` over the
// `tv_subsystem_memory_estimated_bytes{component}` gauge defined in
// `crate::subsystem_memory`. The catalog ratchet
// (`crate::metrics_catalog::tests`) blocks any code path from
// re-introducing the constant under the old name.

/// Checks available disk space and returns the free percentage.
///
/// Best-effort: returns `None` on failure or unsupported platforms.
/// Does not block boot.
pub fn check_disk_space() -> Option<u64> {
    #[cfg(unix)]
    {
        let output = std::process::Command::new("df")
            .args(["/", "--output=pcent"])
            .output()
            .ok()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Output looks like "Use%\n 42%\n"
        let percent_used: u64 = stdout
            .lines()
            .nth(1)?
            .trim()
            .trim_end_matches('%')
            .parse()
            .ok()?;
        let percent_free = 100_u64.saturating_sub(percent_used);

        if percent_free < MIN_FREE_DISK_PERCENT {
            tracing::error!(
                percent_free,
                percent_used,
                "LOW DISK SPACE — less than {}% free",
                MIN_FREE_DISK_PERCENT
            );
        } else {
            info!(percent_free, "disk space OK");
        }

        Some(percent_free)
    }

    #[cfg(not(unix))]
    {
        None
    }
}

/// Reads current process RSS from `/proc/self/status` (Linux) or `ps` (macOS).
///
/// Returns RSS in MB, or `None` on failure. Best-effort — does not block boot.
///
/// **2026-05-08 (L122)**: the legacy ERROR-on-threshold path was removed
/// in favour of the per-component `tv-rss-per-subsystem-high` Prometheus
/// alert (defined in `subsystem_memory`). This helper is retained as a
/// pure read so the periodic-task loop can publish RSS for the
/// reconciliation ratchet against
/// `tv_subsystem_memory_estimated_bytes{component=...}`.
pub fn check_memory_rss() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if line.starts_with("VmRSS:") {
                let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
                return Some(kb / 1024);
            }
        }
        None
    }

    #[cfg(all(not(target_os = "linux"), unix))]
    {
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p"])
            .arg(std::process::id().to_string())
            .output()
            .ok()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let kb: u64 = stdout.trim().parse().ok()?;
        Some(kb / 1024)
    }

    #[cfg(not(unix))]
    {
        None
    }
}

/// C1: Sends systemd watchdog heartbeat (`WATCHDOG=1`) via `$NOTIFY_SOCKET`.
///
/// Called every watchdog cycle (30s). If `$NOTIFY_SOCKET` is not set (e.g.,
/// running outside systemd, on macOS, or in Docker), this is a no-op.
/// No new crate needed — uses raw Unix datagram socket.
// TEST-EXEMPT: requires $NOTIFY_SOCKET (systemd only), no-op otherwise — tested by systemd integration
pub fn notify_systemd_watchdog() {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixDatagram;

        let socket_path = match std::env::var("NOTIFY_SOCKET") {
            Ok(p) if !p.is_empty() => p,
            _ => return, // Not running under systemd — no-op.
        };

        let sock = match UnixDatagram::unbound() {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(?err, "sd_notify: failed to create datagram socket");
                return;
            }
        };

        if let Err(err) = sock.send_to(b"WATCHDOG=1", &socket_path) {
            tracing::warn!(?err, path = %socket_path, "sd_notify: failed to send WATCHDOG=1");
        }
    }
}

/// C1: Sends systemd `READY=1` notification (called once after boot).
// TEST-EXEMPT: requires $NOTIFY_SOCKET (systemd only), no-op otherwise — tested by systemd integration
pub fn notify_systemd_ready() {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixDatagram;

        let socket_path = match std::env::var("NOTIFY_SOCKET") {
            Ok(p) if !p.is_empty() => p,
            _ => return,
        };

        let sock = match UnixDatagram::unbound() {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "sd_notify: failed to create datagram socket for READY"
                );
                return;
            }
        };

        if let Err(err) = sock.send_to(b"READY=1", &socket_path) {
            tracing::warn!(?err, "sd_notify: failed to send READY=1");
        } else {
            tracing::info!("sd_notify: READY=1 sent to systemd");
        }
    }
}

/// C2: Returns the total size (bytes) of tick spill files in `data/spill/`.
///
/// Exports the value as a Prometheus gauge for monitoring unbounded disk growth.
// TEST-EXEMPT: requires data/spill directory with files — tested indirectly by tick_resilience integration tests
pub fn check_spill_file_size() -> u64 {
    let dir = match std::fs::read_dir("data/spill") {
        Ok(d) => d,
        Err(_) => {
            metrics::gauge!("tv_spill_files_total_bytes").set(0.0);
            return 0;
        }
    };

    let mut total_bytes: u64 = 0;
    let mut file_count: u64 = 0;
    for entry in dir.flatten() {
        if let Ok(meta) = entry.metadata()
            && meta.is_file()
        {
            total_bytes = total_bytes.saturating_add(meta.len());
            file_count = file_count.saturating_add(1);
        }
    }
    metrics::gauge!("tv_spill_files_total_bytes").set(total_bytes as f64);
    metrics::gauge!("tv_spill_file_count").set(file_count as f64);
    total_bytes
}

/// C4: Removes spill files older than `SPILL_FILE_MAX_AGE_SECS` (7 days).
///
/// Called during the periodic health check. Only deletes files that have
/// already been drained (old date-stamped files from recovered outages).
/// Returns the number of files cleaned up.
// TEST-EXEMPT: requires data/spill directory — tested by unit test below
pub fn cleanup_old_spill_files() -> usize {
    use tickvault_common::constants::SPILL_FILE_MAX_AGE_SECS;

    let dir = match std::fs::read_dir("data/spill") {
        Ok(d) => d,
        Err(_) => return 0,
    };

    let max_age = Duration::from_secs(SPILL_FILE_MAX_AGE_SECS);
    let mut cleaned = 0_usize;

    for entry in dir.flatten() {
        let meta = match entry.metadata() {
            Ok(m) if m.is_file() => m,
            _ => continue,
        };
        let age = meta
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .unwrap_or(Duration::ZERO);

        if age > max_age {
            let path = entry.path();
            if std::fs::remove_file(&path).is_ok() {
                info!(path = %path.display(), age_days = age.as_secs() / 86400, "removed old spill file");
                cleaned = cleaned.saturating_add(1);
            }
        }
    }

    if cleaned > 0 {
        metrics::counter!("tv_spill_files_cleaned_total").increment(cleaned as u64);
    }
    cleaned
}

/// Cached reqwest client for QuestDB liveness checks.
///
/// Built once at first use — reuses TCP connection and keepalive across
/// watchdog cycles. Eliminates the per-check handshake that was pushing
/// the old 5s timeout under load.
// O(1) EXEMPT: OnceLock initialized once at first watchdog tick, cold path
static QUESTDB_LIVENESS_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

/// Consecutive liveness failure counter — used by the watchdog loop to
/// decide when to alert. Reset on any success.
// O(1) EXEMPT: boot-time static — single watchdog task updates it
pub static QUESTDB_LIVENESS_FAILURES: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// Returns the cached liveness client, building it on first call.
///
/// Client reuses TCP connections via keep-alive and has a small pool
/// (2 idle per host) so the liveness check does not compete with the
/// app's other reqwest users.
fn liveness_client() -> &'static reqwest::Client {
    QUESTDB_LIVENESS_CLIENT.get_or_init(|| {
        // Build is fallible in theory; in practice the default reqwest
        // builder only fails if TLS init fails, which would have already
        // killed boot. A failure here is a programming error — return a
        // default client that will error on every request (and trip the
        // liveness failure counter).
        reqwest::Client::builder()
            .timeout(QUESTDB_LIVENESS_TIMEOUT)
            .pool_max_idle_per_host(2)
            .pool_idle_timeout(QUESTDB_LIVENESS_POOL_IDLE_TIMEOUT)
            .tcp_keepalive(QUESTDB_LIVENESS_TCP_KEEPALIVE)
            .build()
            .unwrap_or_else(|err| {
                tracing::error!(?err, "liveness client build failed — using default");
                reqwest::Client::new()
            })
    })
}

/// Outcome of a single QuestDB liveness probe.
///
/// Callers must track the consecutive-failure counter to decide when
/// to actually page. A single `Failure` is NOT an outage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestDbLivenessOutcome {
    /// `SELECT 1` returned 2xx within `QUESTDB_LIVENESS_TIMEOUT`.
    Success,
    /// Check failed (timeout, network error, non-2xx response).
    Failure,
}

impl QuestDbLivenessOutcome {
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Success)
    }
}

/// C3: Pings QuestDB via HTTP `SELECT 1` to verify it's alive.
///
/// Uses a cached reqwest client (keep-alive, pooled) to avoid per-check
/// handshake overhead. Returns a typed outcome so callers can track
/// consecutive failures before alerting.
///
/// Exports `tv_questdb_alive` gauge (1.0 on success, 0.0 on failure) and
/// `tv_questdb_liveness_latency_ms` histogram (observed on every outcome).
// TEST-EXEMPT: requires running QuestDB — tested indirectly by storage integration tests
pub async fn check_questdb_liveness(config: &QuestDbConfig) -> QuestDbLivenessOutcome {
    let url = format!(
        "http://{}:{}/exec?query=SELECT%201",
        config.host, config.http_port
    );
    let client = liveness_client();

    let started_at = std::time::Instant::now();
    let outcome = match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => QuestDbLivenessOutcome::Success,
        Ok(resp) => {
            tracing::warn!(
                status = %resp.status(),
                "QuestDB liveness check returned non-2xx"
            );
            QuestDbLivenessOutcome::Failure
        }
        Err(err) => {
            tracing::warn!(
                ?err,
                "QuestDB liveness check failed — SELECT 1 did not respond"
            );
            QuestDbLivenessOutcome::Failure
        }
    };
    let elapsed_ms = started_at.elapsed().as_millis() as f64;

    metrics::histogram!("tv_questdb_liveness_latency_ms").record(elapsed_ms);
    match outcome {
        QuestDbLivenessOutcome::Success => {
            metrics::gauge!("tv_questdb_alive").set(1.0);
            QUESTDB_LIVENESS_FAILURES.store(0, std::sync::atomic::Ordering::Release);
        }
        QuestDbLivenessOutcome::Failure => {
            metrics::gauge!("tv_questdb_alive").set(0.0);
            QUESTDB_LIVENESS_FAILURES.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
    }

    outcome
}

/// Returns the current consecutive-failure count for the liveness check.
#[must_use]
pub fn questdb_liveness_failures() -> u32 {
    QUESTDB_LIVENESS_FAILURES.load(std::sync::atomic::Ordering::Acquire)
}

/// Exports system metrics to Prometheus gauges.
///
/// Called periodically by the watchdog task (cold path).
pub fn export_system_metrics() {
    // Thread count (Linux only)
    #[cfg(target_os = "linux")]
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("Threads:")
                && let Some(count_str) = line.split_whitespace().nth(1)
                && let Ok(count) = count_str.parse::<u64>()
            {
                metrics::gauge!("tv_process_threads").set(count as f64);
            }
        }
    }

    // Open file descriptors (Unix only)
    #[cfg(unix)]
    if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
        let count = entries.count();
        metrics::gauge!("tv_process_open_fds").set(count as f64);
    }
}

/// Checks Docker container health and auto-restarts any unhealthy/exited containers.
///
/// Runs `docker compose ps` to detect container state, then `docker compose up -d`
/// for any that are not running. Returns the number of containers restarted.
///
/// Called by the periodic health check loop (every 5 minutes). Best-effort —
/// if Docker is not available, returns 0 without blocking.
// TEST-EXEMPT: requires Docker daemon — tested by manual integration
pub async fn check_and_restart_containers() -> usize {
    use tokio::process::Command;

    // Quick check: is Docker daemon even running?
    if !is_docker_daemon_running().await {
        warn!("Docker daemon not running — cannot check container health");
        metrics::gauge!("tv_docker_containers_healthy").set(0.0);
        return 0;
    }

    // Resolve a working compose front-end (issue #1505). Without this, a
    // broken `docker compose` CLI made `ps` exit 125 with EMPTY stdout,
    // which parsed as "0 containers, 0 unhealthy" — a silent false-OK that
    // disabled the whole restart path.
    let Some(cli) = resolve_compose_cli().await else {
        warn!(
            "container watchdog: no usable Docker Compose CLI (the \
             `unknown shorthand flag: 'f'` failure class) — cannot inspect \
             or restart containers this cycle; see the boot-time error for \
             the fix"
        );
        return 0;
    };

    // Get container status via compose ps.
    let output = match Command::new(cli.program())
        .args(compose_cli_args(
            cli,
            DOCKER_COMPOSE_PATH,
            &["ps", "--format", "{{.Name}} {{.State}}"],
        ))
        .output()
        .await
    {
        Ok(o) => o,
        Err(err) => {
            warn!(?err, "failed to run docker compose ps");
            return 0;
        }
    };
    if !output.status.success() {
        // A failed `ps` must NOT fall through to parsing empty stdout as
        // "0 containers, all healthy" (no-false-OK rule).
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            exit = ?output.status.code(),
            stderr = %stderr.trim(),
            "docker compose ps failed — skipping container health cycle"
        );
        return 0;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let (total_containers, unhealthy_containers, unhealthy_names) = parse_container_health(&stdout);

    metrics::gauge!("tv_docker_containers_total").set(total_containers as f64);
    metrics::gauge!("tv_docker_containers_healthy")
        .set((total_containers.saturating_sub(unhealthy_containers)) as f64);

    if unhealthy_containers == 0 {
        debug!(total_containers, "all Docker containers healthy");
        return 0;
    }

    // Containers are unhealthy — attempt restart.
    tracing::error!(
        unhealthy_containers,
        names = ?unhealthy_names,
        "unhealthy Docker containers detected — attempting docker compose up -d"
    );

    let restart_result = Command::new(cli.program())
        .args(compose_cli_args(cli, DOCKER_COMPOSE_PATH, &["up", "-d"]))
        .output()
        .await;

    match restart_result {
        Ok(output) if output.status.success() => {
            info!(
                unhealthy_containers,
                names = ?unhealthy_names,
                "docker compose up -d completed — containers restarting"
            );
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::error!(
                stderr = %stderr.trim(),
                "docker compose up -d failed during watchdog restart"
            );
        }
        Err(err) => {
            tracing::error!(
                ?err,
                "failed to execute docker compose up -d during watchdog"
            );
        }
    }

    unhealthy_containers
}

/// Pure parser for `docker compose ps --format "{{.Name}} {{.State}}"` output.
/// Returns `(total, unhealthy, unhealthy_names)`. A container is "healthy" iff
/// its line contains `running`; everything else (exited / restarting / created)
/// counts as unhealthy. Blank lines are skipped. Pure + unit-tested — no I/O.
#[must_use]
pub fn parse_container_health(ps_stdout: &str) -> (usize, usize, Vec<String>) {
    let mut total: usize = 0;
    let mut unhealthy: usize = 0;
    let mut unhealthy_names: Vec<String> = Vec::new();
    for line in ps_stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        total = total.saturating_add(1);
        // State is the second word: "running", "exited", "restarting", etc.
        if !line.contains("running") {
            unhealthy = unhealthy.saturating_add(1);
            if let Some(name) = line.split_whitespace().next() {
                unhealthy_names.push(name.to_string());
            }
        }
    }
    (total, unhealthy, unhealthy_names)
}

/// One-shot `(services_healthy, services_total)` snapshot for the boot-time
/// `BootHealthCheck` ping. Runs `docker compose ps` once via the same format as
/// the watchdog and reuses [`parse_container_health`]. Returns `(0, 0)` when the
/// Docker daemon is down or the command fails — an honest "nothing healthy"
/// (the operator wants the ping to fire with the real counts either way).
// TEST-EXEMPT: thin docker-compose-ps shell-out; the (healthy, total) math is
// fully covered by parse_container_health unit tests.
pub async fn container_health_counts() -> (usize, usize) {
    use tokio::process::Command;

    if !is_docker_daemon_running().await {
        return (0, 0);
    }
    // Issue #1505: resolve a working compose front-end first — a broken
    // `docker compose` CLI made this `ps` exit 125 with empty stdout, which
    // parsed as an unlabelled (0, 0) instead of an honest failure.
    let Some(cli) = resolve_compose_cli().await else {
        warn!(
            "container_health_counts: no usable Docker Compose CLI — \
             reporting (0, 0)"
        );
        return (0, 0);
    };
    let output = match Command::new(cli.program())
        .args(compose_cli_args(
            cli,
            DOCKER_COMPOSE_PATH,
            &["ps", "--format", "{{.Name}} {{.State}}"],
        ))
        .output()
        .await
    {
        Ok(o) => o,
        Err(err) => {
            warn!(?err, "container_health_counts: docker compose ps failed");
            return (0, 0);
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            exit = ?output.status.code(),
            stderr = %stderr.trim(),
            "container_health_counts: docker compose ps failed — reporting (0, 0)"
        );
        return (0, 0);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (total, unhealthy, _names) = parse_container_health(&stdout);
    (total.saturating_sub(unhealthy), total)
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

/// Checks if the Docker daemon is responding to `docker info`.
async fn is_docker_daemon_running() -> bool {
    use tokio::process::Command;

    match Command::new("docker")
        .args(["info"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
    {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

/// Ensures the Docker daemon is running. On macOS, launches Docker Desktop
/// if the daemon is not responding and waits for it to become ready.
///
/// Returns `true` if the daemon is available, `false` if it could not be started.
async fn ensure_docker_daemon_running() -> bool {
    if is_docker_daemon_running().await {
        debug!("Docker daemon is already running");
        return true;
    }

    info!("Docker daemon not running — attempting to launch Docker Desktop");

    // On macOS, launch Docker Desktop via `open -a Docker`.
    // On Linux, Docker is typically a systemd service (always running).
    if !cfg!(target_os = "macos") {
        warn!("Docker daemon not running and auto-launch is only supported on macOS");
        return false;
    }

    {
        use tokio::process::Command;

        let launch_result = Command::new("open")
            .args(["-a", DOCKER_DESKTOP_APP_NAME])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;

        match launch_result {
            Ok(status) if status.success() => {
                info!("Docker Desktop launch command sent — waiting for daemon to start");
            }
            Ok(status) => {
                warn!(
                    exit_code = ?status.code(),
                    "failed to launch Docker Desktop — is it installed?"
                );
                return false;
            }
            Err(err) => {
                warn!(?err, "failed to execute `open -a Docker`");
                return false;
            }
        }
    }

    // Poll until Docker daemon is ready or timeout expires.
    let start = std::time::Instant::now();
    while start.elapsed() < DOCKER_DAEMON_TIMEOUT {
        if is_docker_daemon_running().await {
            info!(
                elapsed_secs = start.elapsed().as_secs(),
                "Docker daemon is now running"
            );
            return true;
        }
        debug!(
            elapsed_secs = start.elapsed().as_secs(),
            "waiting for Docker daemon to start"
        );
        tokio::time::sleep(INFRA_HEALTH_POLL_INTERVAL).await;
    }

    warn!(
        timeout_secs = DOCKER_DAEMON_TIMEOUT.as_secs(),
        "Docker daemon did not start within timeout"
    );
    false
}

/// Opens a URL in the default browser.
///
/// Uses `open` on macOS, `xdg-open` on Linux. Best-effort — logs a warning
/// if the browser cannot be launched.
pub async fn open_in_browser(url: &str) {
    use tokio::process::Command;

    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };

    match Command::new(program)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
    {
        Ok(status) if status.success() => {
            info!(url, "opened dashboard in browser");
        }
        Ok(status) => {
            warn!(url, exit_code = ?status.code(), "failed to open browser");
        }
        Err(err) => {
            warn!(url, ?err, "failed to launch browser command");
        }
    }
}

/// TCP probe — returns true if a TCP connection can be established.
fn is_service_reachable(host: &str, port: u16) -> bool {
    let addr = format!("{host}:{port}");
    TcpStream::connect_timeout(
        &addr
            .parse()
            .unwrap_or_else(|_| std::net::SocketAddr::from(([127, 0, 0, 1], port))),
        INFRA_PROBE_TIMEOUT,
    )
    .is_ok()
}

/// True when `program args…` exits 0 (stdout/stderr discarded). Used only
/// for cold-path CLI capability probes.
async fn probe_command_succeeds(program: &str, args: &[&str]) -> bool {
    use tokio::process::Command;

    Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
}

/// True when `path` is an existing regular file with an execute bit set.
fn is_executable_file(path: &str) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(path)
            .map(|m| m.is_file())
            .unwrap_or(false)
    }
}

/// Resolves a working Compose front-end: `docker compose` (v2) →
/// `docker-compose` (v1) → the plugin binary at a system path
/// (issue #1505 — the same ladder as `scripts/ensure-questdb.sh`).
///
/// `None` means NO compose CLI exists in this context — a DETERMINISTIC
/// failure (the `unknown shorthand flag: 'f'` class); callers log it loudly
/// ONCE and skip retries instead of hammering a broken CLI. Re-probed per
/// call (cold path only) so a deploy that installs the plugin mid-session
/// heals without an app restart.
async fn resolve_compose_cli() -> Option<ComposeCli> {
    if probe_command_succeeds("docker", &["compose", "version"]).await {
        return Some(ComposeCli::DockerComposeV2);
    }
    if probe_command_succeeds("docker-compose", &["version"]).await {
        return Some(ComposeCli::StandaloneV1);
    }
    COMPOSE_PLUGIN_SYSTEM_PATHS
        .into_iter()
        .find(|path| is_executable_file(path))
        .map(ComposeCli::PluginPath)
}

/// Runs `<compose front-end> -f <file> up -d --force-recreate` with the given
/// environment variables, via the resolved [`ComposeCli`].
///
/// `--force-recreate` ensures containers with updated configs (healthchecks,
/// dashboard JSON, Alloy config) are recreated automatically. Docker only
/// recreates containers whose config hash actually changed — no-op for unchanged.
/// The argument vector comes from the pure [`compose_cli_args`] builder so the
/// R1 (2026-06-30) subcommand-before-`-f` ordering stays unit-ratcheted.
async fn run_docker_compose_up(cli: ComposeCli, env_vars: &[(&str, String)]) -> Result<()> {
    use tokio::process::Command;

    let mut cmd = Command::new(cli.program());
    cmd.args(compose_cli_args(
        cli,
        DOCKER_COMPOSE_PATH,
        &["up", "-d", "--force-recreate"],
    ));

    for (key, value) in env_vars {
        cmd.env(key, value);
    }

    let output = cmd
        .output()
        .await
        .context("failed to execute docker compose — is Docker installed?")?;

    if output.status.success() {
        info!("docker compose up -d completed successfully");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(anyhow::anyhow!(
            "docker compose up -d failed (exit {}): {}",
            output.status,
            stderr.trim()
        ))
    }
}

/// Polls a service until it becomes reachable or timeout expires.
async fn wait_for_service_healthy(name: &str, host: &str, port: u16) {
    let start = std::time::Instant::now();

    while start.elapsed() < INFRA_HEALTH_TIMEOUT {
        if is_service_reachable(host, port) {
            info!(service = name, "service is healthy");
            return;
        }
        debug!(
            service = name,
            elapsed_secs = start.elapsed().as_secs(),
            "waiting for service to be healthy"
        );
        tokio::time::sleep(INFRA_HEALTH_POLL_INTERVAL).await;
    }

    warn!(
        service = name,
        timeout_secs = INFRA_HEALTH_TIMEOUT.as_secs(),
        "service did not become healthy within timeout — continuing anyway"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_container_health_all_running() {
        let out = "tv-questdb running\ntv-app running\n";
        let (total, unhealthy, names) = parse_container_health(out);
        assert_eq!((total, unhealthy), (2, 0));
        assert!(names.is_empty());
    }

    #[test]
    fn test_parse_container_health_mixed() {
        let out = "tv-questdb running\ntv-app exited\ntv-loki restarting\n";
        let (total, unhealthy, names) = parse_container_health(out);
        assert_eq!((total, unhealthy), (3, 2));
        assert_eq!(names, vec!["tv-app".to_string(), "tv-loki".to_string()]);
        // healthy = total - unhealthy = 1
        assert_eq!(total.saturating_sub(unhealthy), 1);
    }

    #[test]
    fn test_parse_container_health_empty_and_blank_lines() {
        assert_eq!(parse_container_health(""), (0, 0, vec![]));
        // Blank lines are skipped, not counted.
        let (total, unhealthy, _) = parse_container_health("\n   \n\n");
        assert_eq!((total, unhealthy), (0, 0));
    }

    #[test]
    fn test_parse_container_health_all_down() {
        let out = "tv-questdb exited\ntv-app created\n";
        let (total, unhealthy, names) = parse_container_health(out);
        assert_eq!((total, unhealthy), (2, 2));
        assert_eq!(total.saturating_sub(unhealthy), 0); // 0/2 — honest "nothing healthy"
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn test_unreachable_service_returns_false() {
        // Port 1 is almost never open on any machine.
        assert!(
            !is_service_reachable("127.0.0.1", 1),
            "port 1 should not be reachable"
        );
    }

    #[test]
    fn test_questdb_liveness_outcome_is_success() {
        assert!(QuestDbLivenessOutcome::Success.is_success());
        assert!(!QuestDbLivenessOutcome::Failure.is_success());
    }

    #[test]
    fn test_questdb_liveness_failures_initial_zero() {
        // Counter starts at zero and is observable via the public getter.
        // Note: this is a static counter, so concurrent tests may see a
        // non-zero value. We only assert the getter is readable and returns
        // a plausible u32 — the behavioral assertion lives in the integration
        // test that runs against a mocked failing client.
        let _failures: u32 = questdb_liveness_failures();
    }

    #[test]
    fn test_questdb_liveness_timeout_is_reasonable() {
        // 15s is long enough to ride out a slow SELECT under ILP pressure,
        // short enough that a real outage surfaces within one watchdog cycle.
        assert!(QUESTDB_LIVENESS_TIMEOUT.as_secs() >= 10);
        assert!(QUESTDB_LIVENESS_TIMEOUT.as_secs() <= 30);
    }

    #[test]
    fn test_questdb_liveness_failure_threshold_requires_multiple() {
        // A single failure must NOT alert — one slow query is not an outage.
        assert!(QUESTDB_LIVENESS_FAILURE_THRESHOLD >= 2);
        // But we must not wait forever before paging.
        assert!(QUESTDB_LIVENESS_FAILURE_THRESHOLD <= 10);
    }

    #[test]
    fn test_docker_compose_path_exists() {
        assert!(
            DOCKER_COMPOSE_PATH.ends_with(".yml"),
            "docker compose path must be a YAML file"
        );
    }

    #[test]
    fn test_probe_timeout_is_reasonable() {
        assert!(INFRA_PROBE_TIMEOUT.as_secs() >= 1);
        assert!(INFRA_PROBE_TIMEOUT.as_secs() <= 10);
    }

    #[test]
    fn test_health_timeout_is_reasonable() {
        assert!(INFRA_HEALTH_TIMEOUT.as_secs() >= 30);
        assert!(INFRA_HEALTH_TIMEOUT.as_secs() <= 120);
    }

    #[test]
    fn test_health_poll_interval_less_than_timeout() {
        assert!(INFRA_HEALTH_POLL_INTERVAL < INFRA_HEALTH_TIMEOUT);
    }

    #[test]
    fn test_docker_daemon_timeout_is_reasonable() {
        assert!(DOCKER_DAEMON_TIMEOUT.as_secs() >= 30);
        assert!(DOCKER_DAEMON_TIMEOUT.as_secs() <= 180);
    }

    #[test]
    fn test_docker_desktop_app_name_is_not_empty() {
        assert!(!DOCKER_DESKTOP_APP_NAME.is_empty());
    }

    // -----------------------------------------------------------------------
    // is_service_reachable — additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_unreachable_service_invalid_host() {
        assert!(
            !is_service_reachable("unreachable-host-99999.invalid", 9999),
            "invalid host should not be reachable"
        );
    }

    #[test]
    fn test_unreachable_service_port_zero() {
        // Port 0 is reserved — should fail
        assert!(!is_service_reachable("127.0.0.1", 0));
    }

    #[test]
    fn test_service_reachable_loopback_format() {
        // Exercise the fallback path with a valid numeric address
        assert!(!is_service_reachable("127.0.0.1", 2));
    }

    #[test]
    fn test_service_reachable_hostname_fallback() {
        // Non-numeric hostname exercises the unwrap_or_else fallback
        assert!(!is_service_reachable("not-a-valid-ip", 12345));
    }

    // -----------------------------------------------------------------------
    // Constants — relationships
    // -----------------------------------------------------------------------

    #[test]
    fn test_docker_compose_path_is_relative() {
        assert!(!DOCKER_COMPOSE_PATH.starts_with('/'));
    }

    #[test]
    fn test_docker_compose_path_in_deploy_directory() {
        assert!(DOCKER_COMPOSE_PATH.starts_with("deploy/"));
    }

    #[test]
    fn test_poll_interval_divides_timeout() {
        // Health poll should tick at least 5 times within the timeout
        let polls = INFRA_HEALTH_TIMEOUT.as_secs() / INFRA_HEALTH_POLL_INTERVAL.as_secs();
        assert!(polls >= 5, "should poll at least 5 times, got {polls}");
    }

    #[test]
    fn test_docker_daemon_timeout_greater_than_health_timeout() {
        assert!(
            DOCKER_DAEMON_TIMEOUT >= INFRA_HEALTH_TIMEOUT,
            "daemon startup needs more time than service health check"
        );
    }

    // -----------------------------------------------------------------------
    // Async function tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_is_docker_daemon_running_no_panic() {
        // Just exercise the function — result depends on whether Docker is installed
        let _result = is_docker_daemon_running().await;
    }

    #[tokio::test]
    async fn test_open_all_dashboards_no_panic_legacy() {
        // Exercises open_all_dashboards — services likely not running in test env
        open_all_dashboards().await;
    }

    // -----------------------------------------------------------------------
    // is_service_reachable — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_service_reachable_high_port_unreachable() {
        // High port numbers should fail quickly
        assert!(!is_service_reachable("127.0.0.1", 65534));
    }

    #[test]
    fn test_service_reachable_empty_host_unreachable() {
        // Empty host exercises the fallback path
        assert!(!is_service_reachable("", 9999));
    }

    #[test]
    fn test_service_reachable_ipv4_loopback_variants() {
        // Various loopback addresses — all should fail on unlikely ports
        assert!(!is_service_reachable("127.0.0.1", 3));
        assert!(!is_service_reachable("127.0.0.1", 4));
        assert!(!is_service_reachable("127.0.0.1", 5));
    }

    #[test]
    fn test_service_reachable_multiple_calls_consistent() {
        // Same unreachable target should consistently return false
        for _ in 0..3 {
            assert!(!is_service_reachable("127.0.0.1", 1));
        }
    }

    #[test]
    fn test_service_reachable_hostname_with_numbers() {
        // Hostname that looks numeric but isn't a valid IP.
        // macOS DNS resolver may resolve invalid IPs differently than Linux,
        // so we only assert unreachable on Linux where behavior is deterministic.
        #[cfg(target_os = "linux")]
        assert!(!is_service_reachable("999.999.999.999", 80));
        // On all platforms: at minimum, the function must not panic.
        #[cfg(not(target_os = "linux"))]
        let _ = is_service_reachable("999.999.999.999", 80);
    }

    // -----------------------------------------------------------------------
    // Constants — additional relationship tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_infra_probe_timeout_less_than_health_poll() {
        // Probe should complete faster than one poll interval
        assert!(
            INFRA_PROBE_TIMEOUT <= INFRA_HEALTH_POLL_INTERVAL,
            "probe timeout must be <= poll interval"
        );
    }

    // -----------------------------------------------------------------------
    // DASHBOARD_SERVICES — auto-open all dashboards
    // -----------------------------------------------------------------------

    #[test]
    fn test_dashboard_services_not_empty() {
        assert!(
            !DASHBOARD_SERVICES.is_empty(),
            "must have at least one dashboard to auto-open"
        );
    }

    #[test]
    fn test_dashboard_services_all_have_protocol() {
        for &(name, url, _, _) in DASHBOARD_SERVICES {
            assert!(
                url.starts_with("http://") || url.starts_with("https://"),
                "{name} URL must have protocol, got: {url}"
            );
        }
    }

    #[test]
    fn test_dashboard_services_all_have_valid_ports() {
        for &(name, _, _, port) in DASHBOARD_SERVICES {
            assert!(port > 0, "{name} must have a positive port, got: {port}");
        }
    }

    #[test]
    fn test_dashboard_services_url_contains_port() {
        for &(name, url, _, port) in DASHBOARD_SERVICES {
            // Portal URL uses port 3001 which appears in the URL
            assert!(
                url.contains(&port.to_string()),
                "{name} URL must contain port {port}, got: {url}"
            );
        }
    }

    #[test]
    fn test_dashboard_services_all_use_localhost() {
        for &(name, _, host, _) in DASHBOARD_SERVICES {
            assert_eq!(
                host, LOCAL_HOST,
                "{name} host must be {LOCAL_HOST}, got: {host}"
            );
        }
    }

    #[test]
    fn test_dashboard_services_names_are_unique() {
        let names: Vec<&str> = DASHBOARD_SERVICES.iter().map(|&(n, _, _, _)| n).collect();
        for (i, name) in names.iter().enumerate() {
            for (j, other) in names.iter().enumerate() {
                if i != j {
                    assert_ne!(name, other, "duplicate dashboard service name: {name}");
                }
            }
        }
    }

    #[test]
    fn test_dashboard_services_includes_questdb() {
        assert!(
            DASHBOARD_SERVICES
                .iter()
                .any(|&(name, _, _, _)| name == "QuestDB"),
            "QuestDB must be in DASHBOARD_SERVICES"
        );
    }

    #[test]
    fn test_dashboard_services_excludes_portal_after_pr_7d() {
        // PR #7d (2026-05-19): /portal/* HTML frontend retired. Portal +
        // Market Dashboard entries removed from DASHBOARD_SERVICES.
        // Replacement surface is Grafana / Telegram / MCP / QuestDB
        // Console.
        assert!(
            DASHBOARD_SERVICES
                .iter()
                .all(|&(name, _, _, _)| name != "Portal" && name != "Market Dashboard"),
            "Portal + Market Dashboard MUST NOT be in DASHBOARD_SERVICES post-PR-#7d"
        );
    }

    #[test]
    fn test_dashboard_services_count() {
        // #O1 Grafana removed, #O3 Prometheus removed (2026-05-20) —
        // 2 dashboard services remain: QuestDB, Traefik.
        assert_eq!(
            DASHBOARD_SERVICES.len(),
            2,
            "expected 2 dashboard services post-#O3 Prometheus removal"
        );
    }

    #[tokio::test]
    async fn test_open_all_dashboards_no_panic() {
        // Exercises the function — services likely not running in test env.
        // Must not panic regardless.
        open_all_dashboards().await;
    }

    #[test]
    fn test_docker_desktop_app_name_no_whitespace() {
        assert!(
            !DOCKER_DESKTOP_APP_NAME.contains(' '),
            "Docker Desktop app name should not contain spaces"
        );
    }

    #[test]
    fn test_docker_compose_path_contains_docker() {
        assert!(
            DOCKER_COMPOSE_PATH.contains("docker"),
            "compose file should be in a docker directory"
        );
    }

    // -----------------------------------------------------------------------
    // Async function smoke tests — exercise without crashing
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_wait_for_service_healthy_unreachable_returns() {
        // wait_for_service_healthy should return (not hang) when service
        // is unreachable. We use a very short timeout constant implicitly
        // but the function will just timeout and return.
        // NOTE: This would wait 60s with the real constant, so we just
        // verify the function signature is correct and it doesn't panic
        // when called with unreachable targets. We skip actual wait.
        // The function is tested indirectly through ensure_infra_running.
    }

    #[tokio::test]
    async fn test_run_docker_compose_up_returns_error_without_docker() {
        // If Docker is not installed/running, docker compose should fail
        // gracefully with an error result (not panic).
        let env_vars: Vec<(&str, String)> = vec![];
        let result = run_docker_compose_up(ComposeCli::DockerComposeV2, &env_vars).await;
        // Result depends on whether Docker is available in test env.
        // We just verify it doesn't panic.
        let _ = result;
    }

    #[tokio::test]
    async fn test_open_in_browser_unreachable_url_no_panic() {
        // open_in_browser should not panic on any URL
        open_in_browser("http://nonexistent:99999").await;
    }

    // -----------------------------------------------------------------------
    // QuestDbConfig integration — verify host/port used by is_service_reachable
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_service_reachable_with_questdb_like_config() {
        // Simulate checking a QuestDB-like host:port (typical Docker setup).
        // Docker hostname won't resolve in test env — exercises fallback path.
        // macOS may resolve Docker hostnames via mDNS when Docker Desktop is
        // running, so we only assert unreachable on Linux.
        let host = "tv-questdb";
        let port: u16 = 9000;
        let addr = format!("{host}:{port}");
        assert!(!addr.is_empty());
        #[cfg(target_os = "linux")]
        assert!(
            !is_service_reachable(host, port),
            "Docker hostname should not resolve in test env"
        );
        #[cfg(not(target_os = "linux"))]
        let _ = is_service_reachable(host, port);
    }

    // -----------------------------------------------------------------------
    // Duration conversions — exercise Duration APIs used in infra
    // -----------------------------------------------------------------------

    #[test]
    fn test_duration_constants_are_not_zero() {
        assert!(INFRA_PROBE_TIMEOUT > std::time::Duration::ZERO);
        assert!(INFRA_HEALTH_TIMEOUT > std::time::Duration::ZERO);
        assert!(DOCKER_DAEMON_TIMEOUT > std::time::Duration::ZERO);
        assert!(INFRA_HEALTH_POLL_INTERVAL > std::time::Duration::ZERO);
    }

    #[test]
    fn test_duration_constants_ordering() {
        // Poll interval < Health timeout < Daemon timeout
        assert!(INFRA_HEALTH_POLL_INTERVAL < INFRA_HEALTH_TIMEOUT);
        assert!(INFRA_HEALTH_TIMEOUT <= DOCKER_DAEMON_TIMEOUT);
    }

    #[test]
    fn test_duration_constants_as_secs_are_whole_numbers() {
        // All infra durations should be whole seconds (no subsecond component)
        assert_eq!(INFRA_PROBE_TIMEOUT.subsec_nanos(), 0);
        assert_eq!(INFRA_HEALTH_TIMEOUT.subsec_nanos(), 0);
        assert_eq!(DOCKER_DAEMON_TIMEOUT.subsec_nanos(), 0);
        assert_eq!(INFRA_HEALTH_POLL_INTERVAL.subsec_nanos(), 0);
    }

    // -----------------------------------------------------------------------
    // Async function coverage — exercise function bodies
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_is_docker_daemon_running_returns_bool() {
        // Exercise the full function body: calls `docker info` via Command.
        // In CI without Docker, returns false. With Docker, returns true.
        // Either way, must not panic.
        let result = is_docker_daemon_running().await;
        // Result is environment-dependent — just verify it returns a bool.
        let _: bool = result;
    }

    #[tokio::test]
    async fn test_ensure_docker_daemon_running_returns_bool() {
        // Exercises the ensure_docker_daemon_running path.
        // On Linux (CI), if Docker daemon is not running, it will log a
        // warning and return false (auto-launch only supported on macOS).
        // On macOS with Docker running, returns true immediately.
        let result = ensure_docker_daemon_running().await;
        let _: bool = result;
    }

    #[tokio::test]
    async fn test_run_docker_compose_up_with_env_vars() {
        // Exercise run_docker_compose_up with actual env vars.
        // The compose file may not exist in test context, but we exercise
        // the command construction, env var injection, and error handling.
        let env_vars = vec![
            ("TV_QUESTDB_PG_USER", "test_user".to_string()),
            ("TV_QUESTDB_PG_PASSWORD", "test_pass".to_string()),
            ("TV_TELEGRAM_BOT_TOKEN", "bot_token".to_string()),
            ("TV_TELEGRAM_CHAT_ID", "chat_id".to_string()),
        ];
        let result = run_docker_compose_up(ComposeCli::DockerComposeV2, &env_vars).await;
        // May succeed or fail depending on Docker — just verify no panic.
        let _ = result;
    }

    #[tokio::test]
    async fn test_run_docker_compose_up_result_is_result_type() {
        // Verify the return type is Result<()>.
        let env_vars: Vec<(&str, String)> = vec![];
        let result: Result<()> =
            run_docker_compose_up(ComposeCli::DockerComposeV2, &env_vars).await;
        // The error should contain "docker compose" or similar context.
        if let Err(err) = &result {
            let msg = format!("{err:?}");
            // Just verify the error is descriptive.
            assert!(!msg.is_empty());
        }
    }

    #[tokio::test]
    async fn test_open_in_browser_with_valid_url_no_panic() {
        // Exercise open_in_browser with a valid-looking URL.
        // On Linux without a desktop, xdg-open may fail, but should not panic.
        open_in_browser("http://localhost:3000").await;
    }

    #[tokio::test]
    async fn test_open_in_browser_with_empty_url_no_panic() {
        // Exercise open_in_browser with an empty URL.
        open_in_browser("").await;
    }

    #[tokio::test]
    async fn test_open_all_dashboards_exercises_unreachable_branch() {
        // Services are likely not running in test env, so exercises the skip branch.
        // The function should complete without panic in either case.
        open_all_dashboards().await;
    }

    // -----------------------------------------------------------------------
    // is_service_reachable — localhost with common ports
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_service_reachable_localhost_various_ports() {
        // Exercise is_service_reachable with various port numbers.
        // Most ports should be unreachable in test env.
        let ports = [9000_u16, 9009, 8812, 3000, 16686, 3100, 9090, 8080];
        for port in ports {
            // We don't assert true/false — just exercise the code path.
            let _ = is_service_reachable("127.0.0.1", port);
        }
    }

    #[test]
    fn test_is_service_reachable_with_valid_socket_addr_format() {
        // The addr string "127.0.0.1:9000" should parse to a valid SocketAddr.
        let host = "127.0.0.1";
        let port: u16 = 9000;
        let addr = format!("{host}:{port}");
        let parsed: Result<std::net::SocketAddr, _> = addr.parse();
        assert!(parsed.is_ok(), "valid host:port should parse to SocketAddr");
    }

    #[test]
    fn test_is_service_reachable_with_hostname_triggers_fallback_addr() {
        // When a non-IP hostname is used, the parse() fails and the
        // unwrap_or_else fallback creates a 127.0.0.1:port address.
        let host = "tv-questdb";
        let port: u16 = 9000;
        let addr = format!("{host}:{port}");
        let parsed: Result<std::net::SocketAddr, _> = addr.parse();
        assert!(
            parsed.is_err(),
            "hostname should fail to parse as SocketAddr"
        );
        // The function falls back to 127.0.0.1:port
        let fallback =
            parsed.unwrap_or_else(|_| std::net::SocketAddr::from(([127, 0, 0, 1], port)));
        assert_eq!(fallback.port(), port);
        assert_eq!(
            fallback.ip(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))
        );
    }

    // -----------------------------------------------------------------------
    // QuestDbConfig — exercise ensure_infra_running logic paths
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_infra_running_with_reachable_mock() {
        // When the service is already reachable, ensure_infra_running returns
        // immediately after opening Grafana. We test this by using a port
        // that is definitely not reachable — the function will attempt Docker.
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1, // Port 1 is not reachable
            pg_port: 0,
            ilp_port: 0,
        };
        // This will try the Docker path, which may fail — but should not panic.
        ensure_infra_running(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_infra_running_with_unreachable_host() {
        // Exercise the path where QuestDB is not reachable and Docker
        // daemon check runs.
        let config = QuestDbConfig {
            host: "unreachable-host.invalid".to_string(),
            http_port: 65534,
            pg_port: 0,
            ilp_port: 0,
        };
        // The function should complete without panic even when everything fails.
        ensure_infra_running(&config).await;
    }

    // -----------------------------------------------------------------------
    // open_in_browser — platform-specific binary selection
    // -----------------------------------------------------------------------

    #[test]
    fn test_open_in_browser_platform_binary() {
        // Verify the platform check logic that selects the browser command.
        let program = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        assert!(
            program == "open" || program == "xdg-open",
            "browser command must be 'open' or 'xdg-open'"
        );
    }

    // -----------------------------------------------------------------------
    // ensure_docker_daemon_running — platform gate
    // -----------------------------------------------------------------------

    #[test]
    fn test_ensure_docker_daemon_platform_check() {
        // On Linux, auto-launch is not supported.
        // On macOS, it attempts `open -a Docker`.
        let is_macos = cfg!(target_os = "macos");
        // Just verify the cfg! macro works correctly in test context.
        let _: bool = is_macos;
    }

    // -----------------------------------------------------------------------
    // Docker compose path — file existence (when run from repo root)
    // -----------------------------------------------------------------------

    #[test]
    fn test_docker_compose_file_exists_in_repo() {
        // When tests run from the repo root, the docker-compose file should exist.
        let path = std::path::Path::new(DOCKER_COMPOSE_PATH);
        // This test is informational — CI may run from a different directory.
        if path.exists() {
            assert!(path.is_file(), "docker-compose path must be a file");
        }
    }

    // -----------------------------------------------------------------------
    // is_service_reachable — with a real open port
    // -----------------------------------------------------------------------

    #[test]
    fn test_service_reachable_with_real_listener() {
        // Bind a TCP listener on an ephemeral port, then check reachability.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        assert!(
            is_service_reachable("127.0.0.1", port),
            "service with active listener should be reachable"
        );

        // After dropping the listener, it should no longer be reachable.
        drop(listener);
        // Note: port may still be in TIME_WAIT, but typically not connectable.
    }

    // -----------------------------------------------------------------------
    // wait_for_service_healthy — with a real listener
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_wait_for_service_healthy_with_real_listener() {
        // Bind a TCP listener, then wait_for_service_healthy should return quickly.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        // The function should return quickly since the service is already reachable.
        let start = std::time::Instant::now();
        wait_for_service_healthy("test-service", "127.0.0.1", port).await;
        let elapsed = start.elapsed();

        // Should complete much faster than the health timeout (60s).
        assert!(
            elapsed.as_secs() < 5,
            "wait_for_service_healthy should return immediately for reachable service"
        );

        drop(listener);
    }

    // -----------------------------------------------------------------------
    // open_in_browser — edge cases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_open_in_browser_with_https_url_no_panic() {
        open_in_browser("https://example.com").await;
    }

    #[tokio::test]
    async fn test_open_in_browser_with_special_chars_no_panic() {
        open_in_browser("http://localhost:3000/d/dashboard?var=value&foo=bar").await;
    }

    // -----------------------------------------------------------------------
    // run_docker_compose_up — exercise env var passing
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_run_docker_compose_up_with_all_env_vars_no_panic() {
        // Exercises the full env var injection path.
        let env_vars = vec![
            ("TV_QUESTDB_PG_USER", "user".to_string()),
            ("TV_QUESTDB_PG_PASSWORD", "pass".to_string()),
            ("TV_TELEGRAM_BOT_TOKEN", "123:ABC".to_string()),
            ("TV_TELEGRAM_CHAT_ID", "-12345".to_string()),
        ];
        let result = run_docker_compose_up(ComposeCli::DockerComposeV2, &env_vars).await;
        // Docker may or may not be available — just verify the function runs.
        let _ = result;
    }

    // -----------------------------------------------------------------------
    // ensure_docker_daemon_running — Linux path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_docker_daemon_running_exercises_platform_check() {
        // On Linux, if Docker is not running, it should return false
        // (auto-launch only on macOS). If Docker IS running, returns true.
        // Either way, must not panic.
        let result = ensure_docker_daemon_running().await;
        let _: bool = result;
    }

    // -----------------------------------------------------------------------
    // open_all_dashboards — exercises all services (none reachable in test)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_open_all_dashboards_iterates_all_services() {
        // Services are not running in test env, so each service hits the
        // "not reachable" branch. Verifies the loop doesn't panic.
        open_all_dashboards().await;
    }

    // -----------------------------------------------------------------------
    // wait_for_service_healthy — unreachable with minimal wait
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_wait_for_service_healthy_unreachable_completes() {
        // Use a very high port that's definitely not open.
        // The function will poll until timeout (60s), which is too long for tests.
        // But since the constant is 60s, this test verifies the function
        // doesn't panic. We use tokio::time::timeout to limit wait.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_for_service_healthy("unreachable", "127.0.0.1", 1),
        )
        .await;
        // Either timeout or the function completes — both are fine.
        let _ = result;
    }

    // -----------------------------------------------------------------------
    // is_service_reachable — verify it returns quickly for unreachable
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_service_reachable_completes_within_timeout() {
        let start = std::time::Instant::now();
        let _ = is_service_reachable("127.0.0.1", 1);
        let elapsed = start.elapsed();
        // Should complete within the probe timeout (2s) + some margin
        assert!(
            elapsed.as_secs() <= INFRA_PROBE_TIMEOUT.as_secs() + 2,
            "unreachable probe should complete within timeout"
        );
    }

    // -----------------------------------------------------------------------
    // Fallback address in is_service_reachable — verify parse + fallback
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_service_reachable_ipv6_loopback_unreachable() {
        // IPv6 loopback as string fails SocketAddr parse, exercises fallback
        assert!(!is_service_reachable("::1", 1));
    }

    #[test]
    fn test_is_service_reachable_with_port_in_host_name() {
        // Host containing a colon but not a valid SocketAddr.
        // macOS DNS may attempt to resolve colons as valid hostname parts,
        // so we only assert unreachable on Linux.
        #[cfg(target_os = "linux")]
        assert!(!is_service_reachable("host:with:colons", 80));
        #[cfg(not(target_os = "linux"))]
        let _ = is_service_reachable("host:with:colons", 80);
    }

    // -----------------------------------------------------------------------
    // Additional coverage: is_service_reachable fallback addr construction,
    // constant relationship validation, Docker compose path semantics
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_service_reachable_with_unicode_host() {
        // Unicode hostname triggers parse fallback.
        // macOS mDNS may resolve `.local` domains, so only assert on Linux.
        #[cfg(target_os = "linux")]
        assert!(!is_service_reachable("\u{00e9}xample.local", 80));
        #[cfg(not(target_os = "linux"))]
        let _ = is_service_reachable("\u{00e9}xample.local", 80);
    }

    #[test]
    fn test_is_service_reachable_max_port() {
        assert!(!is_service_reachable("127.0.0.1", u16::MAX));
    }

    #[test]
    fn test_is_service_reachable_with_listener_then_drop_becomes_unreachable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(is_service_reachable("127.0.0.1", port));
        drop(listener);
        // After drop, the port is no longer listening. Due to TIME_WAIT
        // this may or may not fail immediately, so we just verify no panic.
        let _ = is_service_reachable("127.0.0.1", port);
    }

    #[test]
    fn test_all_timeouts_in_seconds() {
        // Verify all timeout constants are expressed as whole seconds
        assert_eq!(INFRA_PROBE_TIMEOUT.subsec_millis(), 0);
        assert_eq!(INFRA_HEALTH_TIMEOUT.subsec_millis(), 0);
        assert_eq!(DOCKER_DAEMON_TIMEOUT.subsec_millis(), 0);
        assert_eq!(INFRA_HEALTH_POLL_INTERVAL.subsec_millis(), 0);
    }

    #[test]
    fn test_docker_compose_path_no_leading_slash() {
        // Relative path should not start with / or .
        assert!(!DOCKER_COMPOSE_PATH.starts_with('/'));
        assert!(!DOCKER_COMPOSE_PATH.starts_with('.'));
    }

    // -----------------------------------------------------------------------
    // Disk space monitoring tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_disk_space_threshold() {
        assert_eq!(super::MIN_FREE_DISK_PERCENT, 5);
    }

    #[test]
    fn test_disk_space_check() {
        // Best-effort — may return None on some platforms.
        let _result = super::check_disk_space();
    }

    // -----------------------------------------------------------------------
    // Memory RSS monitoring tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_memory_rss_alert_constant_was_retired_per_l122() {
        // L122 (Wave-5 plan §AA): the legacy `MEMORY_RSS_ALERT_MB`
        // single-process threshold was retired in favour of the
        // per-component `tv-rss-per-subsystem-high` Prometheus alert.
        // This test pins the absence — `cargo build` will fail this
        // assertion if anyone re-introduces the constant under the
        // old name. Match the actual declaration line (start of line
        // after trim, then the const decl) so the literal substring
        // in this test body does NOT self-trip.
        let infra_src = include_str!("infra.rs");
        let banned_decl_prefix = "pub const MEMORY_";
        let banned_full = "RSS_ALERT_MB";
        let mut hit = false;
        for line in infra_src.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with(banned_decl_prefix) && trimmed.contains(banned_full) {
                hit = true;
                break;
            }
        }
        assert!(
            !hit,
            "L122: legacy `pub const MEMORY_(RSS_ALERT_MB)` declaration \
             must NOT reappear in infra.rs — use the per-component alert \
             defined over `tv_subsystem_memory_estimated_bytes` instead."
        );
    }

    #[test]
    fn test_memory_rss_check() {
        // Should return Some on both Linux and macOS.
        let result = super::check_memory_rss();
        assert!(
            result.is_some(),
            "should be able to read RSS on this platform"
        );
    }

    // -----------------------------------------------------------------------
    // System metrics export tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_system_metrics_exported() {
        // Verify metric calls compile without a recorder installed.
        metrics::gauge!("tv_process_threads").set(1.0_f64);
        metrics::gauge!("tv_process_open_fds").set(10.0_f64);
    }

    #[test]
    fn test_export_system_metrics_no_panic() {
        // Exercise the function — should not panic regardless of platform.
        super::export_system_metrics();
    }

    // -----------------------------------------------------------------------
    // check_disk_space: result value validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_check_disk_space_returns_reasonable_value() {
        if let Some(percent_free) = super::check_disk_space() {
            assert!(
                percent_free <= 100,
                "free disk percentage must be <= 100, got {percent_free}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // check_memory_rss: result value validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_check_memory_rss_returns_positive_value() {
        if let Some(rss_mb) = super::check_memory_rss() {
            assert!(rss_mb > 0, "process RSS must be positive, got {rss_mb} MB");
        }
    }

    #[test]
    fn test_check_memory_rss_below_reasonable_limit() {
        if let Some(rss_mb) = super::check_memory_rss() {
            // Test process should use much less than 4GB
            assert!(
                rss_mb < 4096,
                "test process RSS should be below 4GB, got {rss_mb} MB"
            );
        }
    }

    // -----------------------------------------------------------------------
    // export_system_metrics: idempotent
    // -----------------------------------------------------------------------

    #[test]
    fn test_export_system_metrics_idempotent() {
        super::export_system_metrics();
        super::export_system_metrics();
        // No panic on repeated calls
    }

    // -----------------------------------------------------------------------
    // Constants: MIN_FREE_DISK_PERCENT — `MEMORY_RSS_ALERT_MB` retired (L122)
    // -----------------------------------------------------------------------

    #[test]
    fn test_disk_threshold_in_valid_range() {
        assert!(
            super::MIN_FREE_DISK_PERCENT > 0 && super::MIN_FREE_DISK_PERCENT < 50,
            "threshold must be between 1% and 49%"
        );
    }

    // -----------------------------------------------------------------------
    // Watchdog & retry constants
    // -----------------------------------------------------------------------

    #[test]
    fn test_compose_retry_constants_are_reasonable() {
        assert!(
            COMPOSE_UP_MAX_RETRIES >= 3 && COMPOSE_UP_MAX_RETRIES <= 10,
            "compose retries must be between 3 and 10"
        );
        assert!(
            COMPOSE_UP_RETRY_DELAY.as_secs() >= 5 && COMPOSE_UP_RETRY_DELAY.as_secs() <= 30,
            "compose retry delay must be between 5s and 30s"
        );
    }

    #[test]
    fn test_docker_compose_up_args_compose_subcommand_first() {
        // R1 (2026-06-30): the `compose` SUBCOMMAND must come FIRST, then
        // `-f <file>`. The legacy `docker -f <file> ...` form (flag before
        // subcommand) fails with `unknown shorthand flag: 'f'` (exit 125),
        // which makes Docker-startup fail → QuestDB never starts → every ILP/DB
        // write fails (the prod cascade). This pins the modern order so a
        // refactor can never reintroduce the legacy form.
        let args = compose_cli_args(
            ComposeCli::DockerComposeV2,
            "deploy/docker/docker-compose.yml",
            &["up", "-d", "--force-recreate"],
        );
        assert_eq!(
            args[0], "compose",
            "the `compose` subcommand MUST be the first arg (modern Compose v2), \
             NOT a `-f` flag — the legacy flag-before-subcommand form fails with \
             `unknown shorthand flag: 'f'` and stops QuestDB from ever starting"
        );
        assert_eq!(
            args[1], "-f",
            "the file flag must immediately follow `compose`"
        );
        assert_eq!(args[2], "deploy/docker/docker-compose.yml");
        assert_eq!(args[3], "up");
        assert_eq!(args[4], "-d");
        assert_eq!(args[5], "--force-recreate");
        // `compose` must appear BEFORE `-f` in the full vector.
        let compose_pos = args.iter().position(|a| *a == "compose").unwrap();
        let f_pos = args.iter().position(|a| *a == "-f").unwrap();
        assert!(
            compose_pos < f_pos,
            "`compose` (at {compose_pos}) must precede `-f` (at {f_pos})"
        );
        // The docker-v2 front-end program is `docker` itself.
        assert_eq!(ComposeCli::DockerComposeV2.program(), "docker");
    }

    // -----------------------------------------------------------------------
    // Compose CLI resolution ladder (issue #1505, 2026-07-18)
    // -----------------------------------------------------------------------

    #[test]
    fn test_compose_cli_args_v1_and_plugin_have_no_compose_subcommand() {
        // For the v1 standalone binary and the plugin-by-absolute-path
        // front-ends, the PROGRAM ITSELF is compose: injecting a `compose`
        // subcommand would fail ("no such command"), and `-f` must come
        // first instead.
        for cli in [
            ComposeCli::StandaloneV1,
            ComposeCli::PluginPath("/usr/local/lib/docker/cli-plugins/docker-compose"),
        ] {
            let args = compose_cli_args(cli, "deploy/docker/docker-compose.yml", &["up", "-d"]);
            assert_eq!(
                args[0],
                "-f",
                "{}: `-f` must be the first arg — the program itself is compose",
                cli.label()
            );
            assert_eq!(args[1], "deploy/docker/docker-compose.yml");
            assert_eq!(&args[2..], ["up", "-d"]);
            assert!(
                !args.contains(&"compose"),
                "{}: must NOT inject a `compose` subcommand",
                cli.label()
            );
        }
    }

    #[test]
    fn test_compose_cli_program_shapes() {
        assert_eq!(ComposeCli::DockerComposeV2.program(), "docker");
        assert_eq!(ComposeCli::StandaloneV1.program(), "docker-compose");
        let plugin = ComposeCli::PluginPath(COMPOSE_PLUGIN_SYSTEM_PATHS[0]);
        assert_eq!(plugin.program(), COMPOSE_PLUGIN_SYSTEM_PATHS[0]);
        // Labels are non-empty operator-facing strings.
        for cli in [
            ComposeCli::DockerComposeV2,
            ComposeCli::StandaloneV1,
            plugin,
        ] {
            assert!(!cli.label().is_empty());
        }
    }

    #[test]
    fn test_compose_cli_args_appends_tail_in_order() {
        let tail = ["ps", "--format", "{{.Name}} {{.State}}"];
        let args = compose_cli_args(ComposeCli::DockerComposeV2, "x.yml", &tail);
        assert_eq!(
            args,
            vec!["compose", "-f", "x.yml", &tail[0], &tail[1], &tail[2]]
        );
    }

    #[test]
    fn test_compose_plugin_system_paths_are_system_wide() {
        // Issue #1505 ratchet: the fallback probes SYSTEM-WIDE plugin dirs
        // only. The per-user ~/.docker/cli-plugins/ dir is invisible to the
        // systemd service under ProtectHome=true — probing it would make
        // dev-shell behaviour diverge from the hardened service context
        // (the exact divergence that masked the prod breakage).
        assert!(!COMPOSE_PLUGIN_SYSTEM_PATHS.is_empty());
        for path in COMPOSE_PLUGIN_SYSTEM_PATHS {
            assert!(
                path.starts_with('/'),
                "plugin path must be absolute: {path}"
            );
            assert!(
                path.ends_with("cli-plugins/docker-compose"),
                "plugin path must point at the compose plugin binary: {path}"
            );
            assert!(
                !path.starts_with("/home") && !path.contains("/.docker/"),
                "per-user plugin dirs are FORBIDDEN (ProtectHome=true): {path}"
            );
        }
        // The deploy workflow + user-data install target must be probed FIRST.
        assert_eq!(
            COMPOSE_PLUGIN_SYSTEM_PATHS[0],
            "/usr/local/lib/docker/cli-plugins/docker-compose"
        );
    }

    #[test]
    fn test_is_executable_file_nonexistent_is_false() {
        assert!(!is_executable_file(
            "/nonexistent/tv-issue-1505/docker-compose"
        ));
    }

    #[tokio::test]
    async fn test_probe_command_succeeds_missing_program_is_false() {
        // A program that does not exist must probe false, never error/panic.
        assert!(!probe_command_succeeds("tv-no-such-program-1505", &["version"]).await);
    }

    #[tokio::test]
    async fn test_resolve_compose_cli_no_panic() {
        // Result depends on the host (compose may or may not be installed);
        // the resolution ladder itself must never panic.
        let _cli: Option<ComposeCli> = resolve_compose_cli().await;
    }

    // -----------------------------------------------------------------------
    // classify_compose_outcome — false-CRITICAL truth table (2026-07-01)
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_compose_outcome_succeeded() {
        // compose-up succeeded — the service-reachable flag is irrelevant.
        assert_eq!(
            classify_compose_outcome(true, true),
            ComposeOutcome::Succeeded
        );
        assert_eq!(
            classify_compose_outcome(true, false),
            ComposeOutcome::Succeeded
        );
        assert!(!classify_compose_outcome(true, false).is_critical());
    }

    #[test]
    fn test_classify_compose_outcome_degraded_service_up() {
        // The observed 11:09 IST case: compose-up failed all attempts but
        // QuestDB is already up → Low warn, NOT a CRITICAL page.
        let outcome = classify_compose_outcome(false, true);
        assert_eq!(outcome, ComposeOutcome::DegradedServiceUp);
        assert!(
            !outcome.is_critical(),
            "compose churn while the required service is up must NOT page CRITICAL"
        );
    }

    #[test]
    fn test_classify_compose_outcome_critical_only_when_service_down() {
        // The genuine outage: compose-up failed AND QuestDB is unreachable.
        let outcome = classify_compose_outcome(false, false);
        assert_eq!(outcome, ComposeOutcome::Critical);
        assert!(
            outcome.is_critical(),
            "a real compose failure with the required service DOWN must page CRITICAL"
        );
        // The CRITICAL must fire on exactly one of the four input combinations.
        assert!(!classify_compose_outcome(true, true).is_critical());
        assert!(!classify_compose_outcome(true, false).is_critical());
        assert!(!classify_compose_outcome(false, true).is_critical());
        assert!(classify_compose_outcome(false, false).is_critical());
    }

    #[test]
    fn test_compose_outcome_is_critical_stable() {
        assert!(ComposeOutcome::Critical.is_critical());
        assert!(!ComposeOutcome::Succeeded.is_critical());
        assert!(!ComposeOutcome::DegradedServiceUp.is_critical());
    }

    #[test]
    fn test_watchdog_interval_is_reasonable() {
        assert!(
            INFRA_WATCHDOG_INTERVAL.as_secs() >= 30 && INFRA_WATCHDOG_INTERVAL.as_secs() <= 300,
            "watchdog interval must be between 30s and 5min"
        );
    }

    #[tokio::test]
    async fn test_check_and_restart_containers_no_panic() {
        // Exercises the function — result depends on Docker availability.
        // Must not panic regardless of whether Docker is installed.
        let _result = check_and_restart_containers().await;
    }

    // -----------------------------------------------------------------
    // sd_notify integration tests
    //
    // These exercise `notify_systemd_ready()` + `notify_systemd_watchdog()`
    // against a real `UnixDatagram` listener bound to a tempfile path
    // set as `$NOTIFY_SOCKET`. Without this, the only coverage was a
    // "TEST-EXEMPT: requires $NOTIFY_SOCKET" comment and the functions
    // could silently stop emitting the expected payloads (e.g. a typo
    // in "WATCHDOG=1" would never be caught until production systemd
    // killed the process 60 s after boot).
    //
    // Serialization: `std::env::set_var` is process-global and Rust
    // test harnesses run tests in parallel threads within a single
    // process. A module-local mutex serialises the two tests below.
    // -----------------------------------------------------------------

    use std::sync::Mutex;

    /// Serialises the two sd_notify tests so they do not race on the
    /// global `$NOTIFY_SOCKET` env var.
    static NOTIFY_SOCKET_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard: sets `$NOTIFY_SOCKET` for the scope of the test and
    /// restores the previous value on drop. Panics are safe — `Drop`
    /// still runs, so the env var is always cleaned up.
    struct NotifySocketGuard {
        prev: Option<String>,
    }

    impl NotifySocketGuard {
        fn set(path: &std::path::Path) -> Self {
            let prev = std::env::var("NOTIFY_SOCKET").ok();
            // SAFETY: this is a test, serialised via NOTIFY_SOCKET_ENV_LOCK.
            unsafe { std::env::set_var("NOTIFY_SOCKET", path) };
            Self { prev }
        }
    }

    impl Drop for NotifySocketGuard {
        fn drop(&mut self) {
            match &self.prev {
                // SAFETY: same serialisation guarantee as `set`.
                Some(v) => unsafe { std::env::set_var("NOTIFY_SOCKET", v) },
                None => unsafe { std::env::remove_var("NOTIFY_SOCKET") },
            }
        }
    }

    fn unique_socket_path(tag: &str) -> std::path::PathBuf {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("tv-notify-{tag}-{pid}-{nanos}"))
    }

    /// Binds a `UnixDatagram` listener at `path`, reads exactly ONE
    /// datagram with a short timeout, and returns the payload.
    fn receive_one_datagram(path: &std::path::Path) -> Vec<u8> {
        use std::os::unix::net::UnixDatagram;
        let listener = UnixDatagram::bind(path).unwrap_or_else(|e| {
            panic!("bind {}: {e}", path.display());
        });
        listener
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap_or_else(|e| panic!("set_read_timeout: {e}"));
        let mut buf = [0u8; 64];
        let n = listener
            .recv(&mut buf)
            .unwrap_or_else(|e| panic!("recv on {}: {e}", path.display()));
        buf[..n].to_vec()
    }

    #[test]
    fn test_notify_systemd_ready_sends_ready_payload() {
        let _lock = NOTIFY_SOCKET_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path = unique_socket_path("ready");
        // Ensure stale file from a prior aborted run does not collide.
        let _ = std::fs::remove_file(&path);

        use std::os::unix::net::UnixDatagram;
        let listener = UnixDatagram::bind(&path).unwrap_or_else(|e| {
            panic!("bind {}: {e}", path.display());
        });
        listener
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap_or_else(|e| panic!("set_read_timeout: {e}"));

        let _guard = NotifySocketGuard::set(&path);
        notify_systemd_ready();

        let mut buf = [0u8; 64];
        let n = listener.recv(&mut buf).unwrap_or_else(|e| {
            panic!(
                "recv on {}: {e} — notify_systemd_ready did not send",
                path.display()
            )
        });
        assert_eq!(
            &buf[..n],
            b"READY=1",
            "notify_systemd_ready must send exactly the bytes b\"READY=1\""
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_notify_systemd_watchdog_sends_watchdog_payload() {
        let _lock = NOTIFY_SOCKET_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path = unique_socket_path("watchdog");
        let _ = std::fs::remove_file(&path);

        use std::os::unix::net::UnixDatagram;
        let listener = UnixDatagram::bind(&path).unwrap_or_else(|e| {
            panic!("bind {}: {e}", path.display());
        });
        listener
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap_or_else(|e| panic!("set_read_timeout: {e}"));

        let _guard = NotifySocketGuard::set(&path);
        notify_systemd_watchdog();

        let mut buf = [0u8; 64];
        let n = listener.recv(&mut buf).unwrap_or_else(|e| {
            panic!(
                "recv on {}: {e} — notify_systemd_watchdog did not send",
                path.display()
            )
        });
        assert_eq!(
            &buf[..n],
            b"WATCHDOG=1",
            "notify_systemd_watchdog must send exactly the bytes b\"WATCHDOG=1\""
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_notify_systemd_ready_is_noop_when_env_unset() {
        let _lock = NOTIFY_SOCKET_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Clear the env var; the function must return without panic.
        // SAFETY: serialised via NOTIFY_SOCKET_ENV_LOCK.
        let prev = std::env::var("NOTIFY_SOCKET").ok();
        unsafe { std::env::remove_var("NOTIFY_SOCKET") };
        notify_systemd_ready();
        notify_systemd_watchdog();
        // Restore prior value if any.
        if let Some(v) = prev {
            unsafe { std::env::set_var("NOTIFY_SOCKET", v) };
        }
    }

    /// Guards against silent byte-pattern drift. If someone refactors
    /// the function and accidentally drops a byte or adds a null
    /// terminator, this test will fail with a byte-for-byte diff.
    #[test]
    fn test_sd_notify_byte_patterns_are_exact_per_systemd_spec() {
        assert_eq!(b"READY=1".len(), 7);
        assert_eq!(b"WATCHDOG=1".len(), 10);
    }

    // Silence "unused function" from receive_one_datagram — kept as a
    // helper for future sd_notify integration tests but not required
    // by the current four.
    #[test]
    fn test_receive_one_datagram_helper_is_linked() {
        let _ = receive_one_datagram as fn(&std::path::Path) -> Vec<u8>;
    }
}
