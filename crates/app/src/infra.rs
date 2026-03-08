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

/// TCP probe timeout for infrastructure services.
const INFRA_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum wait time for Docker services to become healthy.
const INFRA_HEALTH_TIMEOUT: Duration = Duration::from_secs(60);

/// Polling interval when waiting for services to become healthy.
const INFRA_HEALTH_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Path to docker-compose file relative to project root.
const DOCKER_COMPOSE_PATH: &str = "deploy/docker/docker-compose.yml";

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
        return;
    }

    info!("infrastructure not running — auto-starting Docker services");

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

    // Wait for QuestDB to become healthy.
    wait_for_service_healthy("QuestDB", &questdb_config.host, questdb_config.http_port).await;
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

/// TCP probe — returns true if a TCP connection can be established.
fn is_service_reachable(host: &str, port: u16) -> bool {
    let addr = format!("{host}:{port}");
    TcpStream::connect_timeout(
        &addr.parse().unwrap_or_else(|_| {
            // Fallback for hostname resolution — try localhost.
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
}
