//! `tickvault ensure-questdb`: make the `tv-questdb` container RUNNING.
//!
//! Was `scripts/ensure-questdb.sh` until 2026-10-01 (audit-plan D6d). Run by
//! `tickvault.service` as its `ExecStartPre` (boot self-heal) and by the
//! operator console's restart-questdb and docker-reset actions over SSM.
//! Idempotent, cold path, one run per call.
//!
//! The ladder, unchanged from the script (incident 2026-06-08 explains why
//! each rung exists):
//!
//! 1. Already running: nothing to do.
//! 2. Exists but stopped: `docker start` (the container keeps its own
//!    environment, so no credentials are needed).
//! 3. Gone (a nuke or reset): read the QuestDB PG credentials from SSM, make
//!    sure the pinned image is present (up to three bounded pulls), then
//!    recreate through compose: `docker compose` (v2), then `docker-compose`
//!    (v1), then the compose plugin by absolute path.
//! 4. Last resort, no compose at all: `docker run` a faithful `tv-questdb`.
//!    Refused without credentials, so QuestDB is never started without auth.
//!
//! Every docker and compose call has its own wall-clock bound: under
//! `TimeoutStartSec=infinity` systemd would never kill a hung `ExecStartPre`,
//! so a wedged Docker daemon must fail here instead of holding the app down.
//!
//! Exit codes: `0` = QuestDB is up (or already was); `1` = it could not be
//! brought up. The unit's `ExecStartPre=-` ignores a failure; the app's own
//! QuestDB readiness wait is the real gate.
//!
//! Two deliberate differences from the script:
//! - it also probed a per-user `~/.docker/cli-plugins/docker-compose`. That
//!   path is invisible to the service (`ProtectHome=true`), the deploy
//!   installs the plugin system-wide and fails if it cannot, and `infra.rs`
//!   excludes it for the same reason;
//! - the SSM reads use the app's own credential fetch (region `ap-south-1`,
//!   `TV_ENVIRONMENT`), so `AWS_REGION` no longer moves them.

use std::io::Write as _;
use std::time::Duration;

use secrecy::ExposeSecret as _;
use tickvault_core::auth::types::QuestDbCredentials;

use crate::infra::{COMPOSE_PLUGIN_SYSTEM_PATHS, ComposeCli};

/// The first argument that selects this subcommand.
pub const ENSURE_QUESTDB_SUBCOMMAND: &str = "ensure-questdb";

/// Prefix of every line this subcommand prints. The operator console greps the
/// installed binary for it before calling the subcommand, so a binary from
/// before the port (which would ignore the argument and boot the whole app) is
/// never run with it. Pinned against the console's commands by
/// `crates/app/tests/host_tuning_boot_wiring_guard.rs`.
pub const LOG_PREFIX: &str = "ensure-questdb: ";

const SERVICE: &str = "tv-questdb";

/// Pinned in lockstep with `deploy/docker/docker-compose.yml` (a test below
/// reads that file). Multi-arch OCI index digest: one value serves the x86 dev
/// box and the ARM Graviton prod box. `name:tag@sha256:` is the documented
/// form for `pull` and `run`; the digest wins.
const IMAGE: &str =
    "questdb/questdb:9.3.5@sha256:22ad030544f45a396c743124c928ce33de11666c104f63f69ce4e66e07e7b968";

const DEFAULT_COMPOSE_FILE: &str = "/opt/tickvault/repo/deploy/docker/docker-compose.yml";
const COMPOSE_FILE_VAR: &str = "TV_COMPOSE_FILE";

/// Per docker call: a wedged daemon makes `docker …` hang forever.
const DEFAULT_CALL_TIMEOUT_SECS: u64 = 90;
const CALL_TIMEOUT_VAR: &str = "DOCKER_CALL_TIMEOUT_SECS";
/// Per image pull: a cold box pulls hundreds of MB.
const DEFAULT_PULL_TIMEOUT_SECS: u64 = 300;
const PULL_TIMEOUT_VAR: &str = "DOCKER_PULL_TIMEOUT_SECS";
/// The last-resort `docker run`, which may itself pull.
const DEFAULT_RUN_TIMEOUT_SECS: u64 = 300;
const RUN_TIMEOUT_VAR: &str = "DOCKER_RUN_TIMEOUT_SECS";

const PULL_ATTEMPTS: u32 = 3;
const PULL_RETRY_PAUSE_SECS: u64 = 10;

/// Both SSM reads together, so an unreachable endpoint cannot hold the boot.
const SSM_TIMEOUT_SECS: u64 = 30;

/// The QuestDB console bind for the last-resort `docker run`. Compose reads the
/// same key from the `.env` beside the compose file on its own; the raw run
/// must read it explicitly or a self-heal would silently fall back to
/// localhost and break the console's VPC Lambda.
const HTTP_BIND_VAR: &str = "TV_QDB_HTTP_BIND";
const HTTP_BIND_DOTENV_KEY: &str = "TV_QDB_HTTP_BIND=";
const DEFAULT_HTTP_BIND: &str = "127.0.0.1";

/// Under systemd `PATH` can be the minimal default and miss `/usr/local/bin`,
/// where `docker-compose` (v1) commonly lives. Prepended for every child.
const STANDARD_PATH_DIRS: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

const EXIT_UP: i32 = 0;
const EXIT_FAILED: i32 = 1;

/// True when the process was started as `tickvault ensure-questdb`.
pub fn is_invocation(args: &[String]) -> bool {
    args.get(1).map(String::as_str) == Some(ENSURE_QUESTDB_SUBCOMMAND)
}

/// One line to stdout (journald under the unit, the SSM output under the
/// console). A failed write is ignored: there is nowhere else to report it.
fn emit(line: &str) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "ensure-questdb: {line}").unwrap_or_default();
}

/// What one bounded docker call returned. A timeout or a spawn error is
/// `ok = false`, the same as a non-zero exit.
struct Call {
    ok: bool,
    stdout: String,
}

/// Everything the ladder touches outside this process, so the decisions can
/// be tested without Docker or AWS.
trait Host {
    async fn docker(&self, args: &[&str], env: &[(&str, &str)], timeout: Duration) -> Call;
    async fn compose_available(&self, cli: ComposeCli, path: &str, timeout: Duration) -> bool;
    async fn compose_up(
        &self,
        cli: ComposeCli,
        compose_file: &str,
        env: &[(&str, &str)],
        timeout: Duration,
    ) -> bool;
    async fn pg_credentials(&self) -> Option<QuestDbCredentials>;
    async fn pause(&self, duration: Duration);
}

/// The run's settings, read once from the environment.
#[derive(Debug, PartialEq, Eq)]
struct Settings {
    compose_file: String,
    call_timeout: Duration,
    pull_timeout: Duration,
    run_timeout: Duration,
    http_bind: String,
    path: String,
}

/// A positive whole number of seconds, else the default.
fn timeout_from(raw: Option<String>, default_secs: u64) -> Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(default_secs);
    Duration::from_secs(secs)
}

/// The last `TV_QDB_HTTP_BIND=` line of a `.env` file, like the script's
/// `sed -n 's/^TV_QDB_HTTP_BIND=//p' | tail -1`.
fn dotenv_http_bind(dotenv: &str) -> Option<String> {
    dotenv
        .lines()
        .filter_map(|line| line.strip_prefix(HTTP_BIND_DOTENV_KEY))
        .next_back()
        .map(|v| v.trim().to_string())
}

/// The environment value wins, then the `.env` value, then `127.0.0.1`. An
/// empty value counts as unset; a value that is not an IP address falls back
/// to the default, loudly, rather than failing the last-resort recreate.
fn resolve_http_bind(env_value: Option<String>, dotenv: Option<&str>) -> String {
    let chosen = env_value
        .filter(|v| !v.trim().is_empty())
        .or_else(|| dotenv.and_then(dotenv_http_bind))
        .filter(|v| !v.is_empty());
    match chosen {
        Some(v) if v.trim().parse::<std::net::IpAddr>().is_ok() => v.trim().to_string(),
        Some(v) => {
            emit(&format!(
                "WARN: {HTTP_BIND_VAR}={v:?} is not an IP address — using {DEFAULT_HTTP_BIND}"
            ));
            DEFAULT_HTTP_BIND.to_string()
        }
        None => DEFAULT_HTTP_BIND.to_string(),
    }
}

/// The `.env` beside the compose file.
fn dotenv_path(compose_file: &str) -> std::path::PathBuf {
    std::path::Path::new(compose_file)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(".env")
}

fn hardened_path(existing: Option<String>) -> String {
    match existing.filter(|p| !p.is_empty()) {
        Some(p) => format!("{STANDARD_PATH_DIRS}:{p}"),
        None => STANDARD_PATH_DIRS.to_string(),
    }
}

impl Settings {
    fn resolve(
        var: impl Fn(&str) -> Option<String>,
        read_file: impl Fn(&std::path::Path) -> Option<String>,
    ) -> Self {
        let compose_file = var(COMPOSE_FILE_VAR)
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_COMPOSE_FILE.to_string());
        let dotenv = read_file(&dotenv_path(&compose_file));
        Self {
            call_timeout: timeout_from(var(CALL_TIMEOUT_VAR), DEFAULT_CALL_TIMEOUT_SECS),
            pull_timeout: timeout_from(var(PULL_TIMEOUT_VAR), DEFAULT_PULL_TIMEOUT_SECS),
            run_timeout: timeout_from(var(RUN_TIMEOUT_VAR), DEFAULT_RUN_TIMEOUT_SECS),
            http_bind: resolve_http_bind(var(HTTP_BIND_VAR), dotenv.as_deref()),
            path: hardened_path(var("PATH")),
            compose_file,
        }
    }
}

/// The compose front-ends, in the script's order.
fn compose_ladder() -> impl Iterator<Item = ComposeCli> {
    [ComposeCli::DockerComposeV2, ComposeCli::StandaloneV1]
        .into_iter()
        .chain(
            COMPOSE_PLUGIN_SYSTEM_PATHS
                .into_iter()
                .map(ComposeCli::PluginPath),
        )
}

/// The arguments of the last-resort `docker run`, mirroring the compose spec's
/// ports, volume and limits. The credentials are passed by NAME (`-e NAME`),
/// their values only in the child's environment, never on the command line.
fn docker_run_args(http_bind: &str) -> Vec<String> {
    let console_port = format!("{http_bind}:9000:9000");
    [
        "run",
        "-d",
        "--name",
        SERVICE,
        "--hostname",
        SERVICE,
        "--restart",
        "unless-stopped",
        "-p",
        console_port.as_str(),
        "-p",
        "8812:8812",
        "-p",
        "9009:9009",
        "-p",
        "127.0.0.1:9003:9003",
        "-v",
        "tv-questdb-data:/var/lib/questdb",
        "--shm-size",
        "512m",
        "--memory",
        "2g",
        "-e",
        "QDB_TELEMETRY_ENABLED=false",
        "-e",
        "QDB_METRICS_ENABLED=TRUE",
        "-e",
        "QDB_PG_USER",
        "-e",
        "QDB_PG_PASSWORD",
        IMAGE,
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// Make sure the image is present: skip at once when it is (a deploy must
/// never wait on a pull that is not pending), else up to three bounded pulls.
/// A final failure only warns: a recreate rung fails loudly if the image is
/// truly absent.
async fn ensure_image(host: &impl Host, settings: &Settings, path_env: &[(&str, &str)]) {
    if host
        .docker(
            &["image", "inspect", IMAGE],
            path_env,
            settings.call_timeout,
        )
        .await
        .ok
    {
        emit(&format!(
            "image {IMAGE} already present - skipping pull (nothing pending)"
        ));
        return;
    }
    for attempt in 1..=PULL_ATTEMPTS {
        emit(&format!(
            "pulling {IMAGE} (attempt {attempt}/{PULL_ATTEMPTS}, bounded {}s)",
            settings.pull_timeout.as_secs()
        ));
        if host
            .docker(&["pull", IMAGE], path_env, settings.pull_timeout)
            .await
            .ok
        {
            emit(&format!("image {IMAGE} pulled"));
            return;
        }
        host.pause(Duration::from_secs(PULL_RETRY_PAUSE_SECS)).await;
    }
    emit(&format!(
        "WARN: could not pull {IMAGE} after {PULL_ATTEMPTS} bounded attempts - continuing to the recreate rungs (they fail loudly if the image is truly absent)"
    ));
}

/// Run the ladder. Returns the process exit code.
async fn ensure(host: &impl Host, settings: &Settings) -> i32 {
    let path_env = [("PATH", settings.path.as_str())];
    let call = settings.call_timeout;

    // 1. Already running? Nothing to do.
    let running = host
        .docker(
            &["inspect", "-f", "{{.State.Running}}", SERVICE],
            &path_env,
            call,
        )
        .await;
    if running.ok && running.stdout.trim() == "true" {
        emit(&format!("{SERVICE} already running"));
        return EXIT_UP;
    }

    // 2. Exists but stopped → start it (reuses the container's own env).
    if host.docker(&["inspect", SERVICE], &path_env, call).await.ok {
        if host.docker(&["start", SERVICE], &path_env, call).await.ok {
            emit(&format!("started existing stopped {SERVICE}"));
            return EXIT_UP;
        }
        emit("docker start failed (or timed out); falling through to recreate");
    }

    // 3. Gone → recreate. Both the compose rungs and the docker-run fallback
    //    need the PG credentials.
    let credentials = host.pg_credentials().await;
    if credentials.is_none() {
        emit(
            "WARN: could not read QuestDB PG creds from SSM (/tickvault/<env>/questdb/*); recreate may fail",
        );
    }
    ensure_image(host, settings, &path_env).await;

    // 3a–3c. compose v2 → v1 → plugin by absolute path.
    let (pg_user, pg_pass) = credentials
        .as_ref()
        .map(|c| (c.pg_user.expose_secret(), c.pg_password.expose_secret()))
        .unwrap_or_default();
    let compose_env = [
        ("PATH", settings.path.as_str()),
        ("TV_QUESTDB_PG_USER", pg_user),
        ("TV_QUESTDB_PG_PASSWORD", pg_pass),
    ];
    for cli in compose_ladder() {
        if host.compose_available(cli, &settings.path, call).await
            && host
                .compose_up(cli, &settings.compose_file, &compose_env, call)
                .await
        {
            emit(&format!("recreated via {}", cli.label()));
            return EXIT_UP;
        }
    }

    // 3d. Last resort: `docker run`, never without auth.
    if credentials.is_none() {
        emit("FAILED: no compose available AND no PG creds — refusing to docker-run without auth");
        return EXIT_FAILED;
    }
    emit(&format!(
        "no compose found — recreating {SERVICE} via 'docker run'"
    ));
    let run_env = [
        ("PATH", settings.path.as_str()),
        ("QDB_PG_USER", pg_user),
        ("QDB_PG_PASSWORD", pg_pass),
    ];
    let run_args = docker_run_args(&settings.http_bind);
    let run_args: Vec<&str> = run_args.iter().map(String::as_str).collect();
    if host
        .docker(&run_args, &run_env, settings.run_timeout)
        .await
        .ok
    {
        emit("recreated via 'docker run'");
        return EXIT_UP;
    }

    emit(&format!("FAILED to bring up {SERVICE} by any method"));
    EXIT_FAILED
}

/// The real host: the docker CLI, the shared compose spawn, and SSM. Its
/// methods are thin wrappers over processes and AWS; the ladder that calls
/// them is covered through a scripted host in the tests below.
struct SystemHost;

impl Host for SystemHost {
    async fn docker(&self, args: &[&str], env: &[(&str, &str)], timeout: Duration) -> Call {
        let mut cmd = tokio::process::Command::new("docker");
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        for (key, value) in env {
            cmd.env(key, value);
        }
        match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(output)) => Call {
                ok: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            },
            _ => Call {
                ok: false,
                stdout: String::new(),
            },
        }
    }

    async fn compose_available(&self, cli: ComposeCli, path: &str, timeout: Duration) -> bool {
        match cli {
            ComposeCli::DockerComposeV2 => {
                self.docker(&["compose", "version"], &[("PATH", path)], timeout)
                    .await
                    .ok
            }
            // The script's `command -v docker-compose`.
            ComposeCli::StandaloneV1 => path
                .split(':')
                .any(|dir| crate::infra::is_executable_file(&format!("{dir}/docker-compose"))),
            ComposeCli::PluginPath(plugin) => crate::infra::is_executable_file(plugin),
        }
    }

    async fn compose_up(
        &self,
        cli: ComposeCli,
        compose_file: &str,
        env: &[(&str, &str)],
        timeout: Duration,
    ) -> bool {
        let up = crate::infra::compose_output(cli, compose_file, &["up", "-d", SERVICE], env);
        matches!(
            tokio::time::timeout(timeout, up).await,
            Ok(Ok(output)) if output.status.success()
        )
    }

    async fn pg_credentials(&self) -> Option<QuestDbCredentials> {
        let fetch = tickvault_core::auth::secret_manager::fetch_questdb_credentials();
        match tokio::time::timeout(Duration::from_secs(SSM_TIMEOUT_SECS), fetch).await {
            Ok(Ok(credentials)) => Some(credentials),
            _ => None,
        }
    }

    async fn pause(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// Run the subcommand with the real host and environment. Returns the process
/// exit code.
// TEST-EXEMPT: wires the real host and environment into `ensure`, which the
// tests cover with a scripted host.
pub async fn run() -> i32 {
    let settings = Settings::resolve(
        |name| std::env::var(name).ok(),
        |path| std::fs::read_to_string(path).ok(),
    );
    ensure(&SystemHost, &settings).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    const FIXTURE_USER: &str = "fixture-user";
    const FIXTURE_PASS: &str = "fixture-pass-value";

    /// A scripted host: answers docker calls from a queue matched by prefix,
    /// and records every call and the environment it was given.
    #[derive(Default)]
    struct ScriptedHost {
        answers: RefCell<VecDeque<(String, bool, String)>>,
        available: Vec<ComposeCli>,
        up_ok: Vec<ComposeCli>,
        credentials: bool,
        calls: RefCell<Vec<String>>,
        envs: RefCell<Vec<Vec<(String, String)>>>,
        pauses: RefCell<u32>,
        credential_reads: RefCell<u32>,
    }

    impl ScriptedHost {
        fn answer(self, prefix: &str, ok: bool, stdout: &str) -> Self {
            self.answers
                .borrow_mut()
                .push_back((prefix.to_string(), ok, stdout.to_string()));
            self
        }
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
        fn record_env(&self, env: &[(&str, &str)]) {
            self.envs.borrow_mut().push(
                env.iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
            );
        }
        fn last_env(&self) -> Vec<(String, String)> {
            self.envs.borrow().last().cloned().unwrap_or_default()
        }
    }

    impl Host for ScriptedHost {
        async fn docker(&self, args: &[&str], env: &[(&str, &str)], _timeout: Duration) -> Call {
            let line = args.join(" ");
            self.calls.borrow_mut().push(format!("docker {line}"));
            self.record_env(env);
            let mut answers = self.answers.borrow_mut();
            let found = answers
                .iter()
                .position(|(p, _, _)| line.starts_with(p.as_str()));
            match found.and_then(|i| answers.remove(i)) {
                Some((_, ok, stdout)) => Call { ok, stdout },
                None => Call {
                    ok: false,
                    stdout: String::new(),
                },
            }
        }
        async fn compose_available(&self, cli: ComposeCli, path: &str, _t: Duration) -> bool {
            assert!(path.starts_with(STANDARD_PATH_DIRS));
            self.available.contains(&cli)
        }
        async fn compose_up(
            &self,
            cli: ComposeCli,
            compose_file: &str,
            env: &[(&str, &str)],
            _timeout: Duration,
        ) -> bool {
            self.calls
                .borrow_mut()
                .push(format!("compose-up {} {compose_file}", cli.label()));
            self.record_env(env);
            self.up_ok.contains(&cli)
        }
        async fn pg_credentials(&self) -> Option<QuestDbCredentials> {
            *self.credential_reads.borrow_mut() += 1;
            self.credentials.then(|| QuestDbCredentials {
                pg_user: FIXTURE_USER.into(),
                pg_password: FIXTURE_PASS.into(),
            })
        }
        async fn pause(&self, _duration: Duration) {
            *self.pauses.borrow_mut() += 1;
        }
    }

    fn defaults() -> Settings {
        Settings::resolve(|_| None, |_| None)
    }

    fn v2_works() -> ScriptedHost {
        ScriptedHost {
            available: vec![ComposeCli::DockerComposeV2],
            up_ok: vec![ComposeCli::DockerComposeV2],
            credentials: true,
            ..ScriptedHost::default()
        }
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn test_is_invocation_matches_only_the_first_argument() {
        assert!(is_invocation(&args(&["tickvault", "ensure-questdb"])));
        assert!(!is_invocation(&args(&["tickvault"])));
        assert!(!is_invocation(&args(&["tickvault", "holiday-gate"])));
        assert!(!is_invocation(&args(&["tickvault", "x", "ensure-questdb"])));
        assert!(!is_invocation(&[]));
    }

    #[test]
    fn test_log_prefix_is_what_emit_prints() {
        // The console greps the binary for LOG_PREFIX; emit's format string
        // must carry it verbatim so the bytes are in the binary.
        let src = include_str!("ensure_questdb.rs");
        assert!(src.contains(&format!("writeln!(out, \"{LOG_PREFIX}{{line}}\")")));
        assert!(LOG_PREFIX.starts_with(ENSURE_QUESTDB_SUBCOMMAND));
    }

    #[test]
    fn test_image_matches_the_compose_file() {
        let compose = include_str!("../../../deploy/docker/docker-compose.yml");
        assert!(
            compose.contains(&format!("image: {IMAGE}")),
            "the self-heal image must match deploy/docker/docker-compose.yml"
        );
    }

    #[test]
    fn test_settings_defaults() {
        let s = defaults();
        assert_eq!(s.compose_file, DEFAULT_COMPOSE_FILE);
        assert_eq!(s.call_timeout.as_secs(), DEFAULT_CALL_TIMEOUT_SECS);
        assert_eq!(s.pull_timeout.as_secs(), DEFAULT_PULL_TIMEOUT_SECS);
        assert_eq!(s.run_timeout.as_secs(), DEFAULT_RUN_TIMEOUT_SECS);
        assert_eq!(s.http_bind, DEFAULT_HTTP_BIND);
        assert_eq!(s.path, STANDARD_PATH_DIRS);
    }

    #[test]
    fn test_settings_overrides_and_dotenv_location() {
        let seen = RefCell::new(None);
        let s = Settings::resolve(
            |name| match name {
                "TV_COMPOSE_FILE" => Some("/srv/x/docker-compose.yml".to_string()),
                "DOCKER_CALL_TIMEOUT_SECS" => Some(" 7 ".to_string()),
                "DOCKER_PULL_TIMEOUT_SECS" => Some("0".to_string()),
                "DOCKER_RUN_TIMEOUT_SECS" => Some("junk".to_string()),
                "PATH" => Some("/opt/bin".to_string()),
                _ => None,
            },
            |path| {
                *seen.borrow_mut() = Some(path.to_path_buf());
                Some("TV_QDB_HTTP_BIND=0.0.0.0\n".to_string())
            },
        );
        assert_eq!(s.compose_file, "/srv/x/docker-compose.yml");
        assert_eq!(
            seen.borrow().as_deref(),
            Some(std::path::Path::new("/srv/x/.env"))
        );
        assert_eq!(s.call_timeout.as_secs(), 7);
        assert_eq!(
            s.pull_timeout.as_secs(),
            DEFAULT_PULL_TIMEOUT_SECS,
            "zero is not a timeout"
        );
        assert_eq!(
            s.run_timeout.as_secs(),
            DEFAULT_RUN_TIMEOUT_SECS,
            "junk falls back"
        );
        assert_eq!(s.http_bind, "0.0.0.0");
        assert_eq!(s.path, format!("{STANDARD_PATH_DIRS}:/opt/bin"));
    }

    #[test]
    fn test_http_bind_resolution() {
        let dotenv = "A=1\nTV_QDB_HTTP_BIND=10.0.0.5\n#x\nTV_QDB_HTTP_BIND=0.0.0.0\n";
        assert_eq!(
            dotenv_http_bind(dotenv).as_deref(),
            Some("0.0.0.0"),
            "last line wins"
        );
        assert_eq!(dotenv_http_bind("A=1\n"), None);
        assert_eq!(
            dotenv_http_bind(" TV_QDB_HTTP_BIND=1.2.3.4"),
            None,
            "anchored"
        );
        assert_eq!(
            resolve_http_bind(Some("10.1.1.1".into()), Some(dotenv)),
            "10.1.1.1"
        );
        assert_eq!(
            resolve_http_bind(Some("  ".into()), Some(dotenv)),
            "0.0.0.0"
        );
        assert_eq!(resolve_http_bind(None, Some(dotenv)), "0.0.0.0");
        assert_eq!(
            resolve_http_bind(None, Some("TV_QDB_HTTP_BIND=\n")),
            DEFAULT_HTTP_BIND
        );
        assert_eq!(resolve_http_bind(None, None), DEFAULT_HTTP_BIND);
        assert_eq!(
            resolve_http_bind(Some("evil; rm".into()), None),
            DEFAULT_HTTP_BIND
        );
        assert_eq!(resolve_http_bind(Some("::1".into()), None), "::1");
    }

    #[test]
    fn test_dotenv_path_and_hardened_path() {
        assert_eq!(
            dotenv_path("/a/b/docker-compose.yml"),
            std::path::PathBuf::from("/a/b/.env")
        );
        assert_eq!(hardened_path(None), STANDARD_PATH_DIRS);
        assert_eq!(hardened_path(Some(String::new())), STANDARD_PATH_DIRS);
        assert_eq!(
            hardened_path(Some("/x".into())),
            format!("{STANDARD_PATH_DIRS}:/x")
        );
    }

    #[test]
    fn test_compose_ladder_order_matches_the_script() {
        let ladder: Vec<ComposeCli> = compose_ladder().collect();
        assert_eq!(ladder[0], ComposeCli::DockerComposeV2);
        assert_eq!(ladder[1], ComposeCli::StandaloneV1);
        assert_eq!(
            ladder[2..],
            COMPOSE_PLUGIN_SYSTEM_PATHS.map(ComposeCli::PluginPath)
        );
    }

    #[test]
    fn test_docker_run_args_keep_credentials_off_the_command_line() {
        let a = docker_run_args("0.0.0.0");
        let joined = a.join(" ");
        assert!(joined.starts_with(
            "run -d --name tv-questdb --hostname tv-questdb --restart unless-stopped"
        ));
        assert!(
            joined
                .contains("-p 0.0.0.0:9000:9000 -p 8812:8812 -p 9009:9009 -p 127.0.0.1:9003:9003")
        );
        assert!(joined.contains("-v tv-questdb-data:/var/lib/questdb --shm-size 512m --memory 2g"));
        assert!(joined.contains("-e QDB_PG_USER -e QDB_PG_PASSWORD"));
        assert!(
            a.iter()
                .all(|arg| !(arg.starts_with("QDB_PG_") && arg.contains('='))),
            "credentials are passed by name only"
        );
        assert_eq!(a.last().map(String::as_str), Some(IMAGE));
    }

    #[tokio::test]
    async fn test_running_container_is_a_no_op() {
        let host = ScriptedHost::default().answer("inspect -f", true, "true\n");
        assert_eq!(ensure(&host, &defaults()).await, EXIT_UP);
        assert_eq!(
            host.calls(),
            vec!["docker inspect -f {{.State.Running}} tv-questdb"]
        );
        assert_eq!(*host.credential_reads.borrow(), 0);
        assert!(host.last_env().iter().any(|(k, _)| k == "PATH"));
    }

    #[tokio::test]
    async fn test_stopped_container_is_started_without_credentials() {
        let host = ScriptedHost::default()
            .answer("inspect -f", true, "false\n")
            .answer("inspect tv-questdb", true, "")
            .answer("start", true, "");
        assert_eq!(ensure(&host, &defaults()).await, EXIT_UP);
        assert_eq!(
            host.calls().last().map(String::as_str),
            Some("docker start tv-questdb")
        );
        assert_eq!(
            *host.credential_reads.borrow(),
            0,
            "start needs no SSM read"
        );
    }

    #[tokio::test]
    async fn test_failed_start_falls_through_to_recreate() {
        let host = v2_works()
            .answer("inspect -f", true, "false\n")
            .answer("inspect tv-questdb", true, "")
            .answer("start", false, "")
            .answer("image inspect", true, "");
        assert_eq!(ensure(&host, &defaults()).await, EXIT_UP);
        assert!(
            host.calls()
                .iter()
                .any(|c| c.starts_with("compose-up docker compose (v2 plugin)"))
        );
    }

    #[tokio::test]
    async fn test_timed_out_inspect_counts_as_gone() {
        // `ok = false` with no stdout is what a timeout returns.
        let host = v2_works().answer("image inspect", true, "");
        assert_eq!(ensure(&host, &defaults()).await, EXIT_UP);
        assert!(!host.calls().iter().any(|c| c.starts_with("docker start")));
        assert_eq!(*host.credential_reads.borrow(), 1);
    }

    #[tokio::test]
    async fn test_recreate_passes_credentials_to_compose_and_skips_a_present_image() {
        let host = v2_works().answer("image inspect", true, "");
        assert_eq!(ensure(&host, &defaults()).await, EXIT_UP);
        let calls = host.calls();
        assert!(
            !calls.iter().any(|c| c.starts_with("docker pull")),
            "{calls:?}"
        );
        let env = host.last_env();
        assert!(env.contains(&("TV_QUESTDB_PG_USER".into(), FIXTURE_USER.into())));
        assert!(env.contains(&("TV_QUESTDB_PG_PASSWORD".into(), FIXTURE_PASS.into())));
        assert!(env.iter().any(|(k, _)| k == "PATH"));
        assert!(
            calls
                .last()
                .is_some_and(|c| c.ends_with(DEFAULT_COMPOSE_FILE))
        );
    }

    #[tokio::test]
    async fn test_missing_image_is_pulled_until_one_succeeds() {
        let host = v2_works()
            .answer("pull", false, "")
            .answer("pull", true, "");
        assert_eq!(ensure(&host, &defaults()).await, EXIT_UP);
        let pulls = host
            .calls()
            .iter()
            .filter(|c| c.starts_with("docker pull"))
            .count();
        assert_eq!(pulls, 2, "stops after the first good pull");
        assert_eq!(*host.pauses.borrow(), 1);
    }

    #[tokio::test]
    async fn test_three_failed_pulls_still_try_the_rungs() {
        let host = v2_works();
        assert_eq!(ensure(&host, &defaults()).await, EXIT_UP);
        let pulls = host
            .calls()
            .iter()
            .filter(|c| c.starts_with("docker pull"))
            .count();
        assert_eq!(pulls, 3);
        assert_eq!(*host.pauses.borrow(), 3);
    }

    #[tokio::test]
    async fn test_every_rung_is_tried_in_order_then_docker_run() {
        let host = ScriptedHost {
            available: compose_ladder().collect(),
            credentials: true,
            ..ScriptedHost::default()
        }
        .answer("image inspect", true, "")
        .answer("run -d", true, "");
        assert_eq!(ensure(&host, &defaults()).await, EXIT_UP);
        let ups: Vec<String> = host
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("compose-up"))
            .collect();
        assert_eq!(ups.len(), 2 + COMPOSE_PLUGIN_SYSTEM_PATHS.len());
        assert!(ups[0].contains("v2 plugin") && ups[1].contains("v1 standalone"));
        let env = host.last_env();
        assert!(env.contains(&("QDB_PG_USER".into(), FIXTURE_USER.into())));
        assert!(env.contains(&("QDB_PG_PASSWORD".into(), FIXTURE_PASS.into())));
        let run = host.calls().last().cloned().unwrap_or_default();
        assert!(run.starts_with("docker run -d --name tv-questdb"));
        assert!(
            !run.contains(FIXTURE_PASS),
            "the credential never reaches argv"
        );
    }

    #[tokio::test]
    async fn test_unavailable_front_ends_are_skipped() {
        let plugin = ComposeCli::PluginPath(COMPOSE_PLUGIN_SYSTEM_PATHS[1]);
        let host = ScriptedHost {
            available: vec![plugin],
            up_ok: vec![plugin],
            credentials: true,
            ..ScriptedHost::default()
        }
        .answer("image inspect", true, "");
        assert_eq!(ensure(&host, &defaults()).await, EXIT_UP);
        let ups: Vec<String> = host
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("compose-up"))
            .collect();
        assert_eq!(ups.len(), 1, "only the available front-end runs: {ups:?}");
    }

    #[tokio::test]
    async fn test_no_credentials_refuses_docker_run() {
        let host = ScriptedHost::default().answer("image inspect", true, "");
        assert_eq!(ensure(&host, &defaults()).await, EXIT_FAILED);
        assert!(!host.calls().iter().any(|c| c.starts_with("docker run")));
    }

    #[tokio::test]
    async fn test_no_credentials_still_tries_compose() {
        let host = ScriptedHost {
            available: vec![ComposeCli::DockerComposeV2],
            up_ok: vec![ComposeCli::DockerComposeV2],
            ..ScriptedHost::default()
        }
        .answer("image inspect", true, "");
        assert_eq!(ensure(&host, &defaults()).await, EXIT_UP);
    }

    #[tokio::test]
    async fn test_failed_docker_run_exits_one() {
        let host = ScriptedHost {
            credentials: true,
            ..ScriptedHost::default()
        }
        .answer("image inspect", true, "");
        assert_eq!(ensure(&host, &defaults()).await, EXIT_FAILED);
        assert!(
            host.calls()
                .last()
                .is_some_and(|c| c.starts_with("docker run"))
        );
    }

    #[tokio::test]
    async fn test_docker_run_uses_the_resolved_console_bind() {
        let s = Settings::resolve(
            |n| (n == "TV_QDB_HTTP_BIND").then(|| "0.0.0.0".to_string()),
            |_| None,
        );
        let host = ScriptedHost {
            credentials: true,
            ..ScriptedHost::default()
        }
        .answer("image inspect", true, "")
        .answer("run -d", true, "");
        assert_eq!(ensure(&host, &s).await, EXIT_UP);
        assert!(
            host.calls()
                .last()
                .is_some_and(|c| c.contains("-p 0.0.0.0:9000:9000"))
        );
    }
}
