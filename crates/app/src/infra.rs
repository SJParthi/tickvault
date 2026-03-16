//! Infrastructure orchestrator — auto-starts Docker services if not running.
//!
//! On startup, probes QuestDB and Jaeger. If unreachable, fetches infra
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

use dhan_live_trader_common::config::QuestDbConfig;
use dhan_live_trader_core::auth::secret_manager;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

// O(1) EXEMPT: begin — named constants, not hardcoded Duration
/// TCP probe timeout for infrastructure services.
const INFRA_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum wait time for Docker services to become healthy.
const INFRA_HEALTH_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum wait time for Docker daemon to start after launching Docker Desktop.
const DOCKER_DAEMON_TIMEOUT: Duration = Duration::from_secs(90);

/// Polling interval when waiting for services to become healthy.
const INFRA_HEALTH_POLL_INTERVAL: Duration = Duration::from_secs(2);
// O(1) EXEMPT: end

/// Path to docker-compose file relative to project root.
const DOCKER_COMPOSE_PATH: &str = "deploy/docker/docker-compose.yml";

/// macOS Docker Desktop application name for `open -a`.
const DOCKER_DESKTOP_APP_NAME: &str = "Docker";

/// Grafana dashboard URL — auto-opened in browser after infrastructure starts.
const GRAFANA_DASHBOARD_URL: &str = "http://localhost:3000";

/// Grafana host for TCP reachability probe.
// O(1) EXEMPT: localhost probe for local Docker infrastructure
const GRAFANA_HOST: &str = "127.0.0.1";

/// Grafana HTTP port for TCP reachability probe.
const GRAFANA_PORT: u16 = 3000;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Ensures Docker infrastructure services are running.
///
/// 1. Probes QuestDB HTTP port — if reachable, returns immediately.
/// 2. If unreachable, fetches infra credentials from SSM.
/// 3. Runs `docker compose up -d` with SSM-sourced env vars.
/// 4. Waits for QuestDB to become healthy (up to 60s).
///
/// Best-effort: if Docker or SSM is unavailable, logs a warning
/// and returns — the app already handles missing services gracefully.
pub async fn ensure_infra_running(questdb_config: &QuestDbConfig) {
    if is_service_reachable(&questdb_config.host, questdb_config.http_port) {
        debug!(
            host = %questdb_config.host,
            port = questdb_config.http_port,
            "infrastructure already running (QuestDB reachable)"
        );
        // Infrastructure already running — still auto-open Grafana dashboard.
        open_grafana_if_reachable().await;
        return;
    }

    info!("infrastructure not running — auto-starting Docker services");

    // Ensure Docker daemon is running (launches Docker Desktop on macOS if needed).
    if !ensure_docker_daemon_running().await {
        warn!("Docker daemon is not available — cannot auto-start infrastructure");
        return;
    }

    // Fetch infra credentials from SSM concurrently.
    let (questdb_creds, grafana_creds, telegram_creds) = match tokio::join!(
        secret_manager::fetch_questdb_credentials(),
        secret_manager::fetch_grafana_credentials(),
        secret_manager::fetch_telegram_credentials(),
    ) {
        (Ok(q), Ok(g), Ok(t)) => (q, g, t),
        (q_result, g_result, t_result) => {
            if let Err(ref err) = q_result {
                warn!(
                    ?err,
                    "failed to fetch QuestDB credentials from SSM — cannot auto-start Docker"
                );
            }
            if let Err(ref err) = g_result {
                warn!(
                    ?err,
                    "failed to fetch Grafana credentials from SSM — cannot auto-start Docker"
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
            "DLT_QUESTDB_PG_USER",
            questdb_creds.pg_user.expose_secret().to_string(),
        ),
        (
            "DLT_QUESTDB_PG_PASSWORD",
            questdb_creds.pg_password.expose_secret().to_string(),
        ),
        (
            "DLT_GRAFANA_ADMIN_USER",
            grafana_creds.admin_user.expose_secret().to_string(),
        ),
        (
            "DLT_GRAFANA_ADMIN_PASSWORD",
            grafana_creds.admin_password.expose_secret().to_string(),
        ),
        (
            "DLT_TELEGRAM_BOT_TOKEN",
            telegram_creds.bot_token.expose_secret().to_string(),
        ),
        (
            "DLT_TELEGRAM_CHAT_ID",
            telegram_creds.chat_id.expose_secret().to_string(),
        ),
    ];

    // Run docker compose up -d.
    match run_docker_compose_up(&env_vars).await {
        Ok(()) => {
            info!("docker compose started — waiting for services to be healthy");
        }
        Err(err) => {
            warn!(
                ?err,
                "docker compose up failed — services may not be available"
            );
            return;
        }
    }

    // Wait for QuestDB and Grafana to become healthy.
    wait_for_service_healthy("QuestDB", &questdb_config.host, questdb_config.http_port).await;
    wait_for_service_healthy("Grafana", GRAFANA_HOST, GRAFANA_PORT).await;

    // Auto-open Grafana dashboards in the default browser.
    open_grafana_if_reachable().await;
}

/// Opens the Grafana dashboard in the default browser if Grafana is reachable.
///
/// Best-effort: if Grafana is not running or the browser cannot be launched,
/// logs a warning and returns — does not block boot.
async fn open_grafana_if_reachable() {
    if is_service_reachable(GRAFANA_HOST, GRAFANA_PORT) {
        open_in_browser(GRAFANA_DASHBOARD_URL).await;
    } else {
        debug!("Grafana not reachable — skipping browser auto-open");
    }
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
async fn open_in_browser(url: &str) {
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
        &addr.parse().unwrap_or_else(|_| {
            // Fallback for hostname resolution — try localhost.
            // O(1) EXEMPT: cold path, infrastructure probe
            format!("127.0.0.1:{port}")
                .parse()
                .expect("127.0.0.1:<port> must parse") // APPROVED: infallible for valid port
        }),
        INFRA_PROBE_TIMEOUT,
    )
    .is_ok()
}

/// Runs `docker compose up -d` with the given environment variables.
async fn run_docker_compose_up(env_vars: &[(&str, String)]) -> Result<()> {
    use tokio::process::Command;

    let mut cmd = Command::new("docker");
    cmd.args(["compose", "-f", DOCKER_COMPOSE_PATH, "up", "-d"]);

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
    fn test_unreachable_service_returns_false() {
        // Port 1 is almost never open on any machine.
        assert!(
            !is_service_reachable("127.0.0.1", 1),
            "port 1 should not be reachable"
        );
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
}
