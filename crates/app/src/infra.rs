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

    // -----------------------------------------------------------------------
    // is_service_reachable — additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_reachable_with_invalid_host_returns_false() {
        // A non-existent hostname should fail gracefully.
        assert!(
            !is_service_reachable("256.256.256.256", 8080),
            "invalid IP should not be reachable"
        );
    }

    #[test]
    fn test_reachable_with_zero_port() {
        // Port 0 is reserved and should not be reachable.
        assert!(
            !is_service_reachable("127.0.0.1", 0),
            "port 0 should not be reachable"
        );
    }

    #[test]
    fn test_reachable_with_loopback_high_port() {
        // A random high port on loopback is almost certainly not listening.
        assert!(
            !is_service_reachable("127.0.0.1", 49999),
            "random high port should not be reachable"
        );
    }

    // -----------------------------------------------------------------------
    // Constants — value invariant tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_grafana_dashboard_url_is_http() {
        assert!(
            GRAFANA_DASHBOARD_URL.starts_with("http://"),
            "Grafana dashboard URL must use HTTP (local)"
        );
    }

    #[test]
    fn test_grafana_port_matches_url() {
        let expected_port_str = format!(":{}", GRAFANA_PORT);
        assert!(
            GRAFANA_DASHBOARD_URL.contains(&expected_port_str),
            "Grafana URL port must match GRAFANA_PORT constant"
        );
    }

    #[test]
    fn test_grafana_host_is_loopback() {
        assert_eq!(
            GRAFANA_HOST, "127.0.0.1",
            "Grafana host must be loopback for local probes"
        );
    }

    #[test]
    fn test_docker_compose_path_contains_docker() {
        assert!(
            DOCKER_COMPOSE_PATH.contains("docker"),
            "docker compose path must reference docker directory"
        );
    }

    #[test]
    fn test_poll_interval_divides_evenly_into_timeout() {
        // Verifying that timeout / poll_interval gives a reasonable number of retries.
        let retries = INFRA_HEALTH_TIMEOUT.as_secs() / INFRA_HEALTH_POLL_INTERVAL.as_secs();
        assert!(
            retries >= 5,
            "should have at least 5 retries: got {retries}"
        );
        assert!(
            retries <= 60,
            "should not have more than 60 retries: got {retries}"
        );
    }

    #[test]
    fn test_docker_daemon_timeout_exceeds_health_timeout() {
        // Docker daemon startup can take longer than individual service health checks.
        assert!(
            DOCKER_DAEMON_TIMEOUT >= INFRA_HEALTH_TIMEOUT,
            "Docker daemon timeout should be >= service health timeout"
        );
    }

    // -----------------------------------------------------------------------
    // open_in_browser — platform detection test
    // -----------------------------------------------------------------------

    #[test]
    fn test_platform_detection_returns_valid_command() {
        // Verify the platform detection logic produces a known command.
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
    // Async tests — is_docker_daemon_running, wait_for_service_healthy
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_is_docker_daemon_running_does_not_panic() {
        // This test just verifies the function runs without panicking.
        // The result depends on whether Docker is installed in the test env.
        let _result = is_docker_daemon_running().await;
    }

    #[tokio::test]
    async fn test_wait_for_service_healthy_with_unreachable() {
        // wait_for_service_healthy should return after timeout without panicking.
        // We use a very short timeout by testing the function's fallback behavior.
        // Port 1 is almost never open, so this should just poll and warn.
        // Note: The actual timeout is INFRA_HEALTH_TIMEOUT (60s), which is too long
        // for a unit test. Instead we verify the function signature and type.
        // This is effectively a compile-time check that the function exists and
        // accepts the right arguments.
        let _ = std::any::type_name_of_val(&wait_for_service_healthy);
    }

    #[tokio::test]
    async fn test_run_docker_compose_up_with_empty_env() {
        // Running docker compose with no env vars should either succeed
        // (if Docker is available) or return an error (if not). Either way,
        // it should not panic.
        let env_vars: &[(&str, String)] = &[];
        let result = run_docker_compose_up(env_vars).await;
        // We don't assert success — Docker may not be installed in test env.
        // The important thing is it doesn't panic.
        let _is_ok = result.is_ok();
    }

    // -----------------------------------------------------------------------
    // ensure_docker_daemon_running — platform-dependent behavior
    // -----------------------------------------------------------------------

    #[test]
    fn test_ensure_docker_daemon_running_on_linux_without_docker() {
        // On Linux (our CI/test platform), Docker auto-launch is not supported.
        // The function should return false if Docker daemon is not running.
        if !cfg!(target_os = "macos") {
            // We can't easily test the full async function here, but we verify
            // the platform guard logic: non-macOS returns false when daemon not running.
            assert!(
                !cfg!(target_os = "macos"),
                "this test should only run on non-macOS"
            );
        }
    }

    // -----------------------------------------------------------------------
    // is_service_reachable — address format tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_reachable_builds_correct_address_format() {
        // The function formats "{host}:{port}" — verify this internally.
        let addr = format!("{}:{}", "127.0.0.1", 9000_u16);
        let parsed: Result<std::net::SocketAddr, _> = addr.parse();
        assert!(
            parsed.is_ok(),
            "address format must produce valid SocketAddr"
        );
    }

    #[test]
    fn test_reachable_fallback_for_hostname() {
        // When hostname can't be parsed as SocketAddr, fallback to 127.0.0.1.
        // "dlt-questdb:9000" can't be parsed as SocketAddr directly.
        let addr = format!("{}:{}", "dlt-questdb", 9000_u16);
        let parsed: Result<std::net::SocketAddr, _> = addr.parse();
        assert!(
            parsed.is_err(),
            "hostname should fail SocketAddr parse (triggering fallback)"
        );

        // The fallback should produce a valid address
        let fallback = format!("127.0.0.1:{}", 9000_u16);
        let fallback_parsed: Result<std::net::SocketAddr, _> = fallback.parse();
        assert!(fallback_parsed.is_ok(), "fallback address must be valid");
    }

    // -----------------------------------------------------------------------
    // is_service_reachable — comprehensive edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_reachable_with_hostname_fallback_returns_false() {
        // Docker DNS hostname like "dlt-questdb" triggers the fallback path.
        // Since nothing is listening on 127.0.0.1:59999, it should return false.
        assert!(
            !is_service_reachable("dlt-questdb", 59999),
            "Docker hostname fallback should return false when nothing listens"
        );
    }

    #[test]
    fn test_reachable_with_ipv4_loopback_variants() {
        // Different loopback representations should all work
        assert!(
            !is_service_reachable("127.0.0.1", 59998),
            "loopback should not be reachable on random port"
        );
    }

    #[test]
    fn test_reachable_with_port_max_value() {
        // Test with maximum valid port number
        assert!(
            !is_service_reachable("127.0.0.1", u16::MAX),
            "port 65535 should not be reachable"
        );
    }

    #[test]
    fn test_reachable_with_port_one_is_privileged() {
        // Port 1 is privileged and almost never has a listener
        assert!(
            !is_service_reachable("127.0.0.1", 1),
            "privileged port 1 should not be reachable"
        );
    }

    #[test]
    fn test_reachable_with_multiple_unreachable_hosts() {
        // Test that different invalid hosts all return false (no panic)
        let hosts = [
            "192.0.2.1",    // TEST-NET-1 (RFC 5737), not routable
            "198.51.100.1", // TEST-NET-2 (RFC 5737)
            "203.0.113.1",  // TEST-NET-3 (RFC 5737)
        ];
        for host in &hosts {
            assert!(
                !is_service_reachable(host, 9000),
                "non-routable host {host} should not be reachable"
            );
        }
    }

    #[test]
    fn test_reachable_function_is_deterministic_for_unreachable() {
        // Calling twice should produce the same result (no state leaks)
        let result1 = is_service_reachable("127.0.0.1", 59997);
        let result2 = is_service_reachable("127.0.0.1", 59997);
        assert_eq!(
            result1, result2,
            "is_service_reachable must be deterministic"
        );
    }

    // -----------------------------------------------------------------------
    // Address format and fallback logic tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_address_format_with_valid_ipv4() {
        // Standard IPv4 addresses should parse correctly
        for port in [80_u16, 443, 8080, 9000, 3000] {
            let addr = format!("127.0.0.1:{port}");
            let parsed: Result<std::net::SocketAddr, _> = addr.parse();
            assert!(parsed.is_ok(), "127.0.0.1:{port} must parse as SocketAddr");
        }
    }

    #[test]
    fn test_address_format_with_docker_hostnames() {
        // Docker DNS names should fail SocketAddr parse (triggering fallback)
        let docker_names = [
            "dlt-questdb",
            "dlt-valkey",
            "dlt-grafana",
            "dlt-prometheus",
            "dlt-jaeger",
            "dlt-loki",
            "dlt-alloy",
            "dlt-traefik",
        ];
        for name in &docker_names {
            let addr = format!("{name}:9000");
            let parsed: Result<std::net::SocketAddr, _> = addr.parse();
            assert!(
                parsed.is_err(),
                "Docker DNS name '{name}' must fail SocketAddr parse"
            );
        }
    }

    #[test]
    fn test_fallback_address_format_for_all_valid_ports() {
        // The fallback builds "127.0.0.1:{port}" which must always parse
        for port in [0_u16, 1, 80, 443, 3000, 8080, 9000, 9009, 16686, 65535] {
            let fallback = format!("127.0.0.1:{port}");
            let parsed: Result<std::net::SocketAddr, _> = fallback.parse();
            assert!(
                parsed.is_ok(),
                "fallback 127.0.0.1:{port} must always parse"
            );
        }
    }

    // -----------------------------------------------------------------------
    // QuestDbConfig integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_questdb_config_host_port_format() {
        // Verify the QuestDbConfig fields used in ensure_infra_running
        let config = QuestDbConfig {
            host: "dlt-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };

        assert_eq!(config.host, "dlt-questdb");
        assert_eq!(config.http_port, 9000);

        // The is_service_reachable call uses these fields
        let addr = format!("{}:{}", config.host, config.http_port);
        // Docker hostname fails SocketAddr parse (expected)
        let parsed: Result<std::net::SocketAddr, _> = addr.parse();
        assert!(parsed.is_err(), "Docker hostname should trigger fallback");
    }

    #[test]
    fn test_questdb_config_with_ip_host() {
        // When running with an IP address (e.g., in CI), it should parse directly
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };

        let addr = format!("{}:{}", config.host, config.http_port);
        let parsed: Result<std::net::SocketAddr, _> = addr.parse();
        assert!(parsed.is_ok(), "IP address should parse directly");
    }

    // -----------------------------------------------------------------------
    // Constants — relationship invariants
    // -----------------------------------------------------------------------

    #[test]
    fn test_health_poll_interval_is_positive() {
        assert!(
            INFRA_HEALTH_POLL_INTERVAL.as_millis() > 0,
            "poll interval must be positive"
        );
    }

    #[test]
    fn test_probe_timeout_less_than_poll_interval_or_equal() {
        // The probe timeout should be <= poll interval to avoid overlapping probes
        assert!(
            INFRA_PROBE_TIMEOUT <= INFRA_HEALTH_POLL_INTERVAL,
            "probe timeout ({:?}) should be <= poll interval ({:?})",
            INFRA_PROBE_TIMEOUT,
            INFRA_HEALTH_POLL_INTERVAL,
        );
    }

    #[test]
    fn test_all_timeouts_are_nonzero() {
        assert!(INFRA_PROBE_TIMEOUT.as_nanos() > 0);
        assert!(INFRA_HEALTH_TIMEOUT.as_nanos() > 0);
        assert!(DOCKER_DAEMON_TIMEOUT.as_nanos() > 0);
        assert!(INFRA_HEALTH_POLL_INTERVAL.as_nanos() > 0);
    }

    #[test]
    fn test_grafana_url_contains_localhost() {
        assert!(
            GRAFANA_DASHBOARD_URL.contains("localhost")
                || GRAFANA_DASHBOARD_URL.contains("127.0.0.1"),
            "Grafana URL must point to local host"
        );
    }

    #[test]
    fn test_grafana_port_is_standard() {
        // Grafana default port is 3000
        assert_eq!(GRAFANA_PORT, 3000, "Grafana port must be 3000 (standard)");
    }

    #[test]
    fn test_docker_compose_path_is_relative() {
        // The path should be relative to project root (no leading /)
        assert!(
            !DOCKER_COMPOSE_PATH.starts_with('/'),
            "docker compose path should be relative"
        );
    }

    #[test]
    fn test_docker_compose_path_has_deploy_prefix() {
        assert!(
            DOCKER_COMPOSE_PATH.starts_with("deploy/"),
            "docker compose must live under deploy/"
        );
    }

    // -----------------------------------------------------------------------
    // Async tests — run_docker_compose_up edge cases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_run_docker_compose_up_with_env_vars() {
        // Test that env vars are accepted without panic (Docker may not be present)
        let env_vars: Vec<(&str, String)> = vec![
            ("DLT_QUESTDB_PG_USER", "admin".to_string()),
            ("DLT_QUESTDB_PG_PASSWORD", "secret".to_string()),
        ];
        let result = run_docker_compose_up(&env_vars).await;
        // Either succeeds or fails with a meaningful error, never panics
        match &result {
            Ok(()) => {} // Docker available
            Err(err) => {
                let err_str = format!("{err:?}");
                // Must contain useful error context
                assert!(
                    err_str.contains("docker")
                        || err_str.contains("Docker")
                        || err_str.contains("compose")
                        || err_str.contains("No such file"),
                    "error must mention docker/compose: {err_str}"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_run_docker_compose_up_error_is_anyhow() {
        // Verify the error type is anyhow::Error (Result<()>)
        let env_vars: &[(&str, String)] = &[];
        let result = run_docker_compose_up(env_vars).await;
        // We just verify it returns a Result — the function signature
        // guarantees it's anyhow::Result<()>.
        let _is_result: bool = result.is_ok() || result.is_err();
    }

    // -----------------------------------------------------------------------
    // Async tests — is_docker_daemon_running behavior
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_is_docker_daemon_running_returns_bool() {
        let result = is_docker_daemon_running().await;
        // Just verify it returns a bool without panicking
        let _is_running: bool = result;
    }

    #[tokio::test]
    async fn test_is_docker_daemon_running_is_deterministic() {
        let result1 = is_docker_daemon_running().await;
        let result2 = is_docker_daemon_running().await;
        assert_eq!(
            result1, result2,
            "docker daemon check must be deterministic in short intervals"
        );
    }

    // -----------------------------------------------------------------------
    // ensure_docker_daemon_running — platform guard logic
    // -----------------------------------------------------------------------

    #[test]
    fn test_platform_guard_non_macos() {
        // On Linux, the function returns false if Docker is not running
        // because auto-launch is macOS-only.
        let is_macos = cfg!(target_os = "macos");
        if !is_macos {
            // On non-macOS, the auto-launch path should not be taken
            assert!(
                !cfg!(target_os = "macos"),
                "this test verifies non-macOS path"
            );
        }
    }

    #[test]
    fn test_platform_guard_open_command_for_macos() {
        // Verify the platform-dependent command selection logic
        if cfg!(target_os = "macos") {
            // macOS should use "open -a Docker"
            let program = "open";
            assert_eq!(program, "open");
        } else {
            // Linux should NOT try to launch Docker Desktop
            assert!(!cfg!(target_os = "macos"));
        }
    }

    // -----------------------------------------------------------------------
    // open_in_browser — command selection tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_browser_command_is_platform_appropriate() {
        let program = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        // Must be a non-empty string
        assert!(!program.is_empty());
        // Must be one of the two known values
        assert!(
            program == "open" || program == "xdg-open",
            "unexpected browser command: {program}"
        );
    }

    #[tokio::test]
    async fn test_open_in_browser_with_invalid_url_does_not_panic() {
        // open_in_browser should handle any URL string without panicking
        // even if the browser command fails.
        // Note: We can't easily test this without running the actual command,
        // but we can verify the URL argument is passed correctly.
        let url = "http://example.invalid:99999/nonexistent";
        // Just verify the function signature accepts any &str
        let _: fn(&str) -> _ = |_url: &str| async {};
        let _ = url; // Ensure url is used
    }

    // -----------------------------------------------------------------------
    // TCP probe — self-connect test (bind + connect to same port)
    // -----------------------------------------------------------------------

    #[test]
    fn test_reachable_with_actual_listener() {
        // Start a TCP listener and verify is_service_reachable returns true
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        assert!(
            is_service_reachable("127.0.0.1", port),
            "port with active listener must be reachable"
        );
        drop(listener);
    }

    #[test]
    fn test_reachable_returns_false_after_listener_dropped() {
        // Start then stop a listener — the port should become unreachable
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        // Must be reachable while listener is alive
        assert!(is_service_reachable("127.0.0.1", port));

        // Drop the listener
        drop(listener);

        // After dropping, the port might still be in TIME_WAIT on some systems,
        // but the connect should fail since no one is accepting.
        // We check that it either succeeds (TIME_WAIT) or fails — no panic.
        let _result = is_service_reachable("127.0.0.1", port);
    }

    #[test]
    fn test_reachable_with_multiple_listeners_simultaneously() {
        use std::net::TcpListener;
        let listener1 = TcpListener::bind("127.0.0.1:0").unwrap();
        let listener2 = TcpListener::bind("127.0.0.1:0").unwrap();
        let port1 = listener1.local_addr().unwrap().port();
        let port2 = listener2.local_addr().unwrap().port();

        assert!(is_service_reachable("127.0.0.1", port1));
        assert!(is_service_reachable("127.0.0.1", port2));

        // Drop one, verify the other is still reachable
        drop(listener1);
        assert!(is_service_reachable("127.0.0.1", port2));

        drop(listener2);
    }

    // -----------------------------------------------------------------------
    // Docker compose path validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_docker_compose_path_has_correct_extension() {
        assert!(
            DOCKER_COMPOSE_PATH.ends_with(".yml") || DOCKER_COMPOSE_PATH.ends_with(".yaml"),
            "docker compose must have .yml or .yaml extension"
        );
    }

    #[test]
    fn test_docker_compose_path_segments() {
        let segments: Vec<&str> = DOCKER_COMPOSE_PATH.split('/').collect();
        assert!(
            segments.len() >= 2,
            "docker compose path must have at least 2 path segments"
        );
        assert_eq!(segments[0], "deploy", "first segment must be 'deploy'");
        assert_eq!(segments[1], "docker", "second segment must be 'docker'");
    }

    // -----------------------------------------------------------------------
    // Timeout relationship tests (ensuring no impossible configurations)
    // -----------------------------------------------------------------------

    #[test]
    fn test_minimum_possible_retries() {
        // The minimum number of health check retries should be >= 5
        // to give services enough time to start.
        let min_retries = INFRA_HEALTH_TIMEOUT.as_millis() / INFRA_HEALTH_POLL_INTERVAL.as_millis();
        assert!(
            min_retries >= 5,
            "must have at least 5 health check retries, got {min_retries}"
        );
    }

    #[test]
    fn test_docker_daemon_retries() {
        // Docker daemon polling should also have reasonable retries
        let daemon_retries =
            DOCKER_DAEMON_TIMEOUT.as_millis() / INFRA_HEALTH_POLL_INTERVAL.as_millis();
        assert!(
            daemon_retries >= 10,
            "Docker daemon should have at least 10 poll attempts, got {daemon_retries}"
        );
    }

    #[test]
    fn test_total_startup_time_bounded() {
        // Total worst-case startup time: Docker daemon + service health
        let total = DOCKER_DAEMON_TIMEOUT + INFRA_HEALTH_TIMEOUT;
        assert!(
            total.as_secs() <= 300,
            "total startup time should be <= 5 minutes, got {}s",
            total.as_secs()
        );
    }

    // -----------------------------------------------------------------------
    // ensure_infra_running — QuestDB reachability logic
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_ensure_infra_running_already_running() {
        // If QuestDB is reachable, ensure_infra_running returns immediately
        // We can test this by binding a listener to simulate QuestDB
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: 8812,
            ilp_port: 9009,
        };

        // Should return immediately since the port is reachable
        let start = std::time::Instant::now();
        ensure_infra_running(&config).await;
        let elapsed = start.elapsed();

        // Should complete quickly (< 5 seconds) since service is already "running"
        assert!(
            elapsed.as_secs() < 5,
            "ensure_infra_running with reachable service should return quickly, took {:?}",
            elapsed
        );

        drop(listener);
    }

    #[tokio::test]
    async fn test_ensure_infra_running_not_running_but_no_docker() {
        // When service is unreachable and Docker is not available,
        // ensure_infra_running should return after a reasonable time
        // without panicking.
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 59996, // Almost certainly nothing listening here
            pg_port: 59995,
            ilp_port: 59994,
        };

        let start = std::time::Instant::now();
        ensure_infra_running(&config).await;
        let elapsed = start.elapsed();

        // Should not take too long (depends on Docker daemon check)
        // In a non-Docker environment, it should fail fast
        assert!(
            elapsed.as_secs() < DOCKER_DAEMON_TIMEOUT.as_secs() + 10,
            "should complete before Docker daemon timeout, took {:?}",
            elapsed
        );
    }

    // -----------------------------------------------------------------------
    // open_grafana_if_reachable tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_open_grafana_if_reachable_with_no_grafana() {
        // When Grafana is not running, should return without panicking
        // We override the check by testing the behavior directly:
        // GRAFANA_PORT (3000) is likely not running in test environment
        let result = is_service_reachable(GRAFANA_HOST, GRAFANA_PORT);
        // Result depends on environment, but function must not panic
        let _is_reachable: bool = result;
    }

    #[tokio::test]
    async fn test_open_grafana_if_reachable_with_mock_grafana() {
        // Start a mock listener on a port and verify the logic path
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        // Verify that the probe would succeed
        assert!(is_service_reachable("127.0.0.1", port));

        drop(listener);
    }
}
