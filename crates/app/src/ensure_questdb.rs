//! `tickvault ensure-questdb` — make the `tv-questdb` container RUNNING.
//!
//! Plan item D6d (rust-only lock §0.10): this subcommand REPLACES the shell
//! script `scripts/ensure-questdb.sh` with the same ladder, the same log lines,
//! the same timeouts (overridable by the same environment variables) and the
//! same exit contract — `0` QuestDB is up (or already was), `1` it could not be
//! brought up.
//!
//! Callers: the systemd unit's `ExecStartPre=-/opt/tickvault/bin/tickvault
//! ensure-questdb` (boot self-heal), and the operator console's
//! `restart-questdb` / `docker-reset` SSM command lists.
//!
//! # The ladder (unchanged from the script)
//!
//! | # | Rung | On success | On failure |
//! |---|---|---|---|
//! | 1 | `docker inspect -f '{{.State.Running}}'` prints `true` | exit 0 | next |
//! | 2 | `docker inspect` (exists) → `docker start` | exit 0 | recreate |
//! | 3 | QuestDB PG credentials from SSM | — | warn, continue |
//! | 3-pre | image present? else `docker pull` × 3 (10 s apart) | — | warn, continue |
//! | 3a | `docker compose version` → `docker compose -f … up -d tv-questdb` | exit 0 | next |
//! | 3b | `docker-compose` on `PATH` → `docker-compose -f … up -d tv-questdb` | exit 0 | next |
//! | 3c | compose plugin by absolute path (4 paths) → `<plugin> -f … up -d` | exit 0 | next |
//! | 3d | no credentials → exit 1; else `docker run` a faithful `tv-questdb` | exit 0 | exit 1 |
//!
//! # Why the incident history still matters (2026-06-08)
//!
//! The ladder exists because a bare `docker compose -f … up -d questdb` had
//! three bugs: hosts without the v2 plugin made it a no-op, the service is
//! named `tv-questdb` (not `questdb`), and a recreate needs the PG credentials
//! the systemd unit's environment does not carry. Every rung is bounded by a
//! hard wall-clock timeout because the unit runs with `TimeoutStartSec=infinity`,
//! so systemd will never kill a hung `ExecStartPre` on a wedged Docker daemon.
//!
//! # Shape
//!
//! [`Ladder`] is the PURE decision core: given the step just run and whether
//! it succeeded, it returns the next step and the line to log. Every branch is
//! unit-tested, and a depth-first walk over every success/failure permutation
//! proves the walk always terminates. [`run_ensure_questdb`] is the thin I/O
//! layer that runs each step with `tokio::process` under `tokio::time::timeout`
//! (the child is killed when the bound expires).
//!
//! # Complexity
//!
//! Cold path, runs once per boot or operator action: at most 21 steps
//! (asserted by `ladder_terminates_on_every_outcome_permutation`), each a
//! bounded child process. Not on any tick path.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use tokio::process::Command;

use crate::infra::{COMPOSE_PLUGIN_SYSTEM_PATHS, ComposeCli, compose_cli_args, is_executable_file};

/// The positional argument that selects this subcommand: `tickvault ensure-questdb`.
pub const ENSURE_QUESTDB_SUBCOMMAND: &str = "ensure-questdb";

/// Container AND compose service name.
const SVC: &str = "tv-questdb";

/// Digest-pinned in lockstep with `deploy/docker/docker-compose.yml`
/// (ratcheted by `image_matches_the_compose_file_pin`). Multi-arch OCI image
/// INDEX digest, so one digest serves the x86 dev box and the ARM prod box.
/// `name:tag@sha256:` is the documented Docker CLI form for `pull` / `run`.
const IMAGE: &str =
    "questdb/questdb:9.3.5@sha256:22ad030544f45a396c743124c928ce33de11666c104f63f69ce4e66e07e7b968";

/// Compose file on the deployed box; `TV_COMPOSE_FILE` overrides it.
const DEFAULT_COMPOSE_FILE: &str = "/opt/tickvault/repo/deploy/docker/docker-compose.yml";

/// Prepended to `PATH` for every child (2026-07-13 service-user fix): under
/// systemd `PATH` can miss `/usr/local/bin`, where `docker-compose` (v1)
/// commonly lives, which silently skipped rungs in the boot context.
const PATH_PREFIX: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Per-call bound for every blocking `docker` / compose call (2026-06-30).
const CALL_TIMEOUT_ENV: &str = "DOCKER_CALL_TIMEOUT_SECS";
const DEFAULT_CALL_TIMEOUT_SECS: u64 = 90;

/// Bound for one `docker pull` attempt (2026-07-13 deploy-hang fix): a
/// cold-box pull of hundreds of MB routinely exceeds the 90 s call bound.
const PULL_TIMEOUT_ENV: &str = "DOCKER_PULL_TIMEOUT_SECS";
const DEFAULT_PULL_TIMEOUT_SECS: u64 = 300;

/// Bound for the last-resort `docker run -d` (it may pull on a cold box).
const RUN_TIMEOUT_ENV: &str = "DOCKER_RUN_TIMEOUT_SECS";
const DEFAULT_RUN_TIMEOUT_SECS: u64 = 300;

/// Pull attempts before falling through to the recreate rungs.
const PULL_ATTEMPTS: u8 = 3;

// O(1) EXEMPT: begin — named constant, not a hardcoded Duration
/// Breather after each failed pull attempt (the script's `sleep 10`).
const PULL_RETRY_PAUSE: Duration = Duration::from_secs(10);
// O(1) EXEMPT: end

/// Bind address for QuestDB's HTTP port 9000 when neither the environment nor
/// the compose project's `.env` sets `TV_QDB_HTTP_BIND` (dev default: local
/// only; prod `.env` sets `0.0.0.0`, admitted only from the console Lambda's SG).
const DEFAULT_HTTP_BIND: &str = "127.0.0.1"; // APPROVED: docker port-publish bind, mirrors the compose default

/// `${HOME:-/home/ec2-user}` in the script's rung 3c.
const DEFAULT_HOME: &str = "/home/ec2-user";

/// Per-user compose plugin location under `$HOME` (rung 3c, fourth path).
const HOME_PLUGIN_SUFFIX: &str = ".docker/cli-plugins/docker-compose";

/// Upper bound on ladder steps; a walk that exceeds it is a decision-core bug.
const MAX_STEPS: usize = 32;

// ---------------------------------------------------------------------------
// Pure core
// ---------------------------------------------------------------------------

/// True when the process was invoked as `tickvault ensure-questdb`.
///
/// Positional: the FIRST argument after the program name must be exactly the
/// subcommand, so a stray `ensure-questdb` later in some other invocation can
/// never divert a normal boot into the self-heal.
#[must_use]
pub fn is_ensure_questdb_invocation<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .nth(1)
        .is_some_and(|arg| arg.as_ref() == ENSURE_QUESTDB_SUBCOMMAND)
}

/// Final verdict of the ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Up,
    Failed,
}

impl Verdict {
    /// The script's exit contract: 0 up, 1 failed.
    fn exit_code(self) -> i32 {
        match self {
            Verdict::Up => 0,
            Verdict::Failed => 1,
        }
    }
}

/// One rung of the ladder. `Done` is terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// `docker inspect -f '{{.State.Running}}' tv-questdb` printed `true`.
    ProbeRunning,
    /// `docker inspect tv-questdb` — the container exists.
    ProbeExists,
    /// `docker start tv-questdb`.
    StartExisting,
    /// Both PG credentials read from SSM and non-empty.
    FetchCreds,
    /// `docker image inspect <IMAGE>`.
    ProbeImage,
    /// `docker pull <IMAGE>`, attempt `n` of [`PULL_ATTEMPTS`].
    Pull(u8),
    /// `docker compose version`.
    ProbeComposeV2,
    /// `docker compose -f <file> up -d tv-questdb`.
    ComposeV2Up,
    /// `docker-compose` resolvable on `PATH` (the script's `command -v`).
    ProbeComposeV1,
    /// `docker-compose -f <file> up -d tv-questdb`.
    ComposeV1Up,
    /// Plugin candidate `i` is an executable file.
    ProbePlugin(usize),
    /// `<plugin i> -f <file> up -d tv-questdb`.
    PluginUp(usize),
    /// Last resort `docker run -d … <IMAGE>`.
    DockerRun,
    /// Terminal.
    Done(Verdict),
}

/// The result of feeding one step outcome to the [`Ladder`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct Transition {
    next: Step,
    say: Option<String>,
}

/// The pure decision core. Holds only what later decisions depend on.
#[derive(Debug, Clone)]
struct Ladder {
    /// Rung 3c candidates, in probe order.
    plugin_paths: Vec<String>,
    /// SSM environment named in the missing-credentials warning.
    ssm_env: String,
    /// Pull bound named in the pull log line.
    pull_timeout: Duration,
    /// Set by the `FetchCreds` outcome; gates the `docker run` rung.
    creds_present: bool,
}

impl Ladder {
    fn new(plugin_paths: Vec<String>, ssm_env: String, pull_timeout: Duration) -> Self {
        Self {
            plugin_paths,
            ssm_env,
            pull_timeout,
            creds_present: false,
        }
    }

    /// The line logged BEFORE a step runs (the script logs these two up front).
    fn announce(&self, step: Step) -> Option<String> {
        match step {
            Step::Pull(attempt) => Some(format!(
                "pulling {IMAGE} (attempt {attempt}/{PULL_ATTEMPTS}, bounded {}s)",
                self.pull_timeout.as_secs_f64()
            )),
            Step::DockerRun => Some(format!(
                "no compose found — recreating {SVC} via 'docker run'"
            )),
            _ => None,
        }
    }

    /// Next step after `step` finished with `ok`, plus the line to log.
    fn transition(&mut self, step: Step, ok: bool) -> Transition {
        let go = |next: Step| Transition { next, say: None };
        let say = |next: Step, line: String| Transition {
            next,
            say: Some(line),
        };
        match (step, ok) {
            (Step::ProbeRunning, true) => {
                say(Step::Done(Verdict::Up), format!("{SVC} already running"))
            }
            (Step::ProbeRunning, false) => go(Step::ProbeExists),
            (Step::ProbeExists, true) => go(Step::StartExisting),
            (Step::ProbeExists, false) => go(Step::FetchCreds),
            (Step::StartExisting, true) => say(
                Step::Done(Verdict::Up),
                format!("started existing stopped {SVC}"),
            ),
            (Step::StartExisting, false) => say(
                Step::FetchCreds,
                "docker start failed (or timed out); falling through to recreate".to_string(),
            ),
            (Step::FetchCreds, true) => {
                self.creds_present = true;
                go(Step::ProbeImage)
            }
            (Step::FetchCreds, false) => {
                self.creds_present = false;
                say(
                    Step::ProbeImage,
                    format!(
                        "WARN: could not read QuestDB PG creds from SSM (/tickvault/{}/questdb/*); recreate may fail",
                        self.ssm_env
                    ),
                )
            }
            (Step::ProbeImage, true) => say(
                Step::ProbeComposeV2,
                format!("image {IMAGE} already present - skipping pull (nothing pending)"),
            ),
            (Step::ProbeImage, false) => go(Step::Pull(1)),
            (Step::Pull(_), true) => say(Step::ProbeComposeV2, format!("image {IMAGE} pulled")),
            (Step::Pull(attempt), false) if attempt < PULL_ATTEMPTS => go(Step::Pull(attempt + 1)),
            (Step::Pull(_), false) => say(
                Step::ProbeComposeV2,
                format!(
                    "WARN: could not pull {IMAGE} after {PULL_ATTEMPTS} bounded attempts - continuing to the recreate rungs (they fail loudly if the image is truly absent)"
                ),
            ),
            (Step::ProbeComposeV2, true) => go(Step::ComposeV2Up),
            (Step::ProbeComposeV2, false) => go(Step::ProbeComposeV1),
            (Step::ComposeV2Up, true) => say(
                Step::Done(Verdict::Up),
                "recreated via 'docker compose' (v2)".to_string(),
            ),
            (Step::ComposeV2Up, false) => go(Step::ProbeComposeV1),
            (Step::ProbeComposeV1, true) => go(Step::ComposeV1Up),
            (Step::ProbeComposeV1, false) => self.plugin_or_after(0),
            (Step::ComposeV1Up, true) => say(
                Step::Done(Verdict::Up),
                "recreated via 'docker-compose' (v1)".to_string(),
            ),
            (Step::ComposeV1Up, false) => self.plugin_or_after(0),
            (Step::ProbePlugin(i), true) => go(Step::PluginUp(i)),
            (Step::ProbePlugin(i), false) => self.plugin_or_after(i.saturating_add(1)),
            (Step::PluginUp(i), true) => {
                let path = self.plugin_paths.get(i).map_or("", String::as_str);
                say(
                    Step::Done(Verdict::Up),
                    format!("recreated via compose plugin at {path}"),
                )
            }
            (Step::PluginUp(i), false) => self.plugin_or_after(i.saturating_add(1)),
            (Step::DockerRun, true) => say(
                Step::Done(Verdict::Up),
                "recreated via 'docker run'".to_string(),
            ),
            (Step::DockerRun, false) => say(
                Step::Done(Verdict::Failed),
                format!("FAILED to bring up {SVC} by any method"),
            ),
            (Step::Done(verdict), _) => go(Step::Done(verdict)),
        }
    }

    /// Plugin candidate `i` if it exists, else the `docker run` gate.
    fn plugin_or_after(&self, i: usize) -> Transition {
        if i < self.plugin_paths.len() {
            return Transition {
                next: Step::ProbePlugin(i),
                say: None,
            };
        }
        if self.creds_present {
            Transition {
                next: Step::DockerRun,
                say: None,
            }
        } else {
            Transition {
                next: Step::Done(Verdict::Failed),
                say: Some(
                    "FAILED: no compose available AND no PG creds — refusing to docker-run without auth"
                        .to_string(),
                ),
            }
        }
    }
}

/// Pause to take AFTER a step, before the next: the script sleeps 10 s after
/// every failed pull attempt, the third included.
fn pause_after(step: Step, ok: bool) -> Option<Duration> {
    match (step, ok) {
        (Step::Pull(_), false) => Some(PULL_RETRY_PAUSE),
        _ => None,
    }
}

/// A timeout from the environment: the script passes the raw value to
/// `timeout "<v>s"`. Unset or empty → the default. A positive number (integer
/// or decimal, as `timeout` accepts) is used as given. Anything else — which
/// made the script's `timeout` refuse to run the call at all — falls back to
/// the default rather than failing every rung.
/// A value too large to be a `Duration` also falls back, never panics.
fn timeout_from(raw: Option<&str>, default_secs: u64) -> Duration {
    raw.map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|secs| secs.is_finite() && *secs > 0.0)
        .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
        .unwrap_or(Duration::from_secs(default_secs))
}

/// `PATH` for every child: the standard dirs first, then the inherited value.
fn effective_path(inherited: Option<&str>) -> String {
    match inherited {
        Some(rest) => format!("{PATH_PREFIX}:{rest}"),
        None => PATH_PREFIX.to_string(),
    }
}

/// Rung 3c candidates: the three system-wide plugin dirs `infra.rs` already
/// probes, then `${HOME:-/home/ec2-user}/.docker/cli-plugins/docker-compose`.
///
/// The fourth (per-user) path is kept because the script probed it: it is
/// invisible under the systemd unit's `ProtectHome=true`, but it is reachable
/// from the operator console's SSM (root) shell, which is the other caller.
fn compose_plugin_candidates(home: Option<&str>) -> Vec<String> {
    let home = home.filter(|h| !h.is_empty()).unwrap_or(DEFAULT_HOME);
    let mut paths: Vec<String> = COMPOSE_PLUGIN_SYSTEM_PATHS
        .iter()
        .map(|p| (*p).to_string())
        .collect();
    paths.push(format!(
        "{}/{HOME_PLUGIN_SUFFIX}",
        home.trim_end_matches('/')
    ));
    paths
}

/// Allowlist check made AT the one variable-program spawn site: the program
/// must be one of this process's own rung 3c candidates, and an absolute path
/// to a file named `docker-compose`. Nothing outside that set is ever spawned.
fn is_allowed_plugin_program(program: &str, candidates: &[String]) -> bool {
    program.starts_with('/')
        && program.ends_with("/docker-compose")
        && !program.contains("..")
        && candidates.iter().any(|c| c == program)
}

/// The script's `[ "$(docker inspect -f '{{.State.Running}}' …)" = "true" ]`:
/// command substitution strips trailing newlines and nothing else.
fn running_probe_says_true(stdout: &[u8]) -> bool {
    let mut end = stdout.len();
    while end > 0 && stdout[end - 1] == b'\n' {
        end -= 1;
    }
    stdout.get(..end) == Some(b"true".as_slice())
}

/// `sed -n 's/^TV_QDB_HTTP_BIND=//p' <.env> | tail -1`: the rest of the LAST
/// line that starts with `TV_QDB_HTTP_BIND=`, byte for byte (no quote or
/// whitespace stripping, exactly as `sed` printed it).
fn http_bind_from_dotenv(dotenv: &str) -> Option<String> {
    dotenv
        .split('\n')
        .rev()
        .find_map(|line| line.strip_prefix("TV_QDB_HTTP_BIND="))
        .map(str::to_string)
}

/// `TV_QDB_HTTP_BIND` from the environment when non-empty, else from the
/// compose project's `.env`, else the local-only default (B4 console, 2026-07-03).
fn resolve_http_bind(env_value: Option<&str>, dotenv: Option<&str>) -> String {
    if let Some(v) = env_value.filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    dotenv
        .and_then(http_bind_from_dotenv)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_HTTP_BIND.to_string())
}

/// The compose rung's argument vector: `[compose] -f <file> up -d tv-questdb`.
/// Built by the shared `infra::compose_cli_args`, so the R1 subcommand-before-
/// `-f` ordering is the same one the boot path is ratcheted on. For the v1
/// binary and the plugin-by-path the program itself is compose, so both take
/// the no-subcommand shape.
fn compose_up_args(v2: bool, compose_file: &str) -> Vec<String> {
    let cli = if v2 {
        ComposeCli::DockerComposeV2
    } else {
        ComposeCli::StandaloneV1
    };
    compose_cli_args(cli, compose_file, &["up", "-d", SVC])
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// The last-resort `docker run` argument vector — the compose spec's ports,
/// volume, limits and environment. The PG credentials are passed with the
/// value-less `-e NAME` form: the values travel in the child's environment
/// only, never on its command line.
fn docker_run_args(http_bind: &str) -> Vec<String> {
    [
        "run",
        "-d",
        "--name",
        SVC,
        "--hostname",
        SVC,
        "--restart",
        "unless-stopped",
        "-p",
        &format!("{http_bind}:9000:9000"),
        "-p",
        "8812:8812",
        "-p",
        "9009:9009",
        "-p",
        "127.0.0.1:9003:9003", // APPROVED: metrics port stays local-only, as in the compose spec
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

/// `command -v <name>` over a `PATH` value: the first directory holding an
/// executable file of that name. An empty entry means the current directory.
fn find_on_path(name: &str, path: &str) -> Option<String> {
    path.split(':').find_map(|dir| {
        let dir = if dir.is_empty() { "." } else { dir };
        let candidate = format!("{}/{name}", dir.trim_end_matches('/'));
        is_executable_file(&candidate).then_some(candidate)
    })
}

/// Everything the I/O layer reads from the environment, read once.
#[derive(Debug, Clone)]
struct Settings {
    compose_file: String,
    call_timeout: Duration,
    pull_timeout: Duration,
    run_timeout: Duration,
    path: String,
    home: Option<String>,
    http_bind_env: Option<String>,
}

impl Settings {
    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let secs = |key: &str, default: u64| timeout_from(lookup(key).as_deref(), default);
        Self {
            compose_file: lookup("TV_COMPOSE_FILE")
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_COMPOSE_FILE.to_string()),
            call_timeout: secs(CALL_TIMEOUT_ENV, DEFAULT_CALL_TIMEOUT_SECS),
            pull_timeout: secs(PULL_TIMEOUT_ENV, DEFAULT_PULL_TIMEOUT_SECS),
            run_timeout: secs(RUN_TIMEOUT_ENV, DEFAULT_RUN_TIMEOUT_SECS),
            path: effective_path(lookup("PATH").as_deref()),
            home: lookup("HOME"),
            http_bind_env: lookup("TV_QDB_HTTP_BIND"),
        }
    }
}

// ---------------------------------------------------------------------------
// I/O layer
// ---------------------------------------------------------------------------

/// Where a child's stdout / stderr go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sink {
    /// `>/dev/null`.
    Null,
    /// Into the journal, as the script left compose / `docker run` stderr.
    Inherit,
    /// Captured (only the running probe reads its output).
    Capture,
}

impl Sink {
    fn stdio(self) -> Stdio {
        match self {
            Sink::Null => Stdio::null(),
            Sink::Inherit => Stdio::inherit(),
            Sink::Capture => Stdio::piped(),
        }
    }
}

/// Outcome of one bounded child process.
#[derive(Debug, Default)]
struct RunResult {
    success: bool,
    stdout: Vec<u8>,
}

/// Runs `cmd` with a hard wall-clock bound. A spawn error, a non-zero exit and
/// a timeout all read as failure, exactly as the script's `timeout` exit 124
/// did. On timeout the future holding the child is dropped and
/// `kill_on_drop` kills the process, so a wedged daemon cannot hang the unit.
async fn run_bounded(mut cmd: Command, limit: Duration, stdout: Sink, stderr: Sink) -> RunResult {
    cmd.stdin(Stdio::null())
        .stdout(stdout.stdio())
        .stderr(stderr.stdio())
        .kill_on_drop(true);
    let Ok(child) = cmd.spawn() else {
        return RunResult::default();
    };
    match tokio::time::timeout(limit, child.wait_with_output()).await {
        Ok(Ok(out)) => RunResult {
            success: out.status.success(),
            stdout: out.stdout,
        },
        Ok(Err(_)) | Err(_) => RunResult::default(),
    }
}

/// A `docker …` child with the hardened `PATH`.
fn docker(settings: &Settings, args: &[&str]) -> Command {
    let mut cmd = Command::new("docker");
    cmd.args(args).env("PATH", &settings.path);
    cmd
}

/// Both PG credentials, held as secrets; never logged.
struct PgCreds {
    user: SecretString,
    password: SecretString,
}

/// Reads the PG credentials from SSM through the shared secret manager
/// (`/tickvault/<env>/questdb/{pg-user,pg-password}`), bounded by the call
/// timeout. `None` on any failure or an empty value — the script treated an
/// empty read as missing.
async fn fetch_creds(limit: Duration) -> Option<PgCreds> {
    let fetched = tokio::time::timeout(
        limit,
        tickvault_core::auth::secret_manager::fetch_questdb_credentials(),
    )
    .await;
    let creds = match fetched {
        Ok(Ok(creds)) => creds,
        Ok(Err(_)) | Err(_) => return None,
    };
    if creds.pg_user.expose_secret().is_empty() || creds.pg_password.expose_secret().is_empty() {
        return None;
    }
    Some(PgCreds {
        user: creds.pg_user,
        password: creds.pg_password,
    })
}

/// The environment the compose rungs carry: the script exported both
/// variables (empty when SSM failed), so a compose recreate interpolates the
/// `${TV_QUESTDB_PG_*:?}` required keys from them.
fn with_compose_env(mut cmd: Command, creds: Option<&PgCreds>) -> Command {
    let (user, password) = match creds {
        Some(c) => (c.user.expose_secret(), c.password.expose_secret()),
        None => ("", ""),
    };
    cmd.env("TV_QUESTDB_PG_USER", user)
        .env("TV_QUESTDB_PG_PASSWORD", password);
    cmd
}

/// Runs one step; `true` = the step's success condition held.
async fn execute(
    step: Step,
    settings: &Settings,
    ladder: &Ladder,
    creds: &mut Option<PgCreds>,
) -> bool {
    match step {
        Step::ProbeRunning => {
            let out = run_bounded(
                docker(settings, &["inspect", "-f", "{{.State.Running}}", SVC]),
                settings.call_timeout,
                Sink::Capture,
                Sink::Null,
            )
            .await;
            running_probe_says_true(&out.stdout)
        }
        Step::ProbeExists => quiet(settings, &["inspect", SVC], settings.call_timeout).await,
        Step::StartExisting => quiet(settings, &["start", SVC], settings.call_timeout).await,
        Step::FetchCreds => {
            *creds = fetch_creds(settings.call_timeout).await;
            creds.is_some()
        }
        Step::ProbeImage => {
            quiet(
                settings,
                &["image", "inspect", IMAGE],
                settings.call_timeout,
            )
            .await
        }
        Step::Pull(_) => quiet(settings, &["pull", IMAGE], settings.pull_timeout).await,
        Step::ProbeComposeV2 => {
            quiet(settings, &["compose", "version"], settings.call_timeout).await
        }
        Step::ComposeV2Up => {
            let args = compose_up_args(true, &settings.compose_file);
            let mut cmd = Command::new("docker");
            cmd.args(&args).env("PATH", &settings.path);
            let cmd = with_compose_env(cmd, creds.as_ref());
            run_bounded(cmd, settings.call_timeout, Sink::Inherit, Sink::Inherit)
                .await
                .success
        }
        Step::ProbeComposeV1 => find_on_path("docker-compose", &settings.path).is_some(),
        Step::ComposeV1Up => {
            let args = compose_up_args(false, &settings.compose_file);
            let mut cmd = Command::new("docker-compose");
            cmd.args(&args).env("PATH", &settings.path);
            let cmd = with_compose_env(cmd, creds.as_ref());
            run_bounded(cmd, settings.call_timeout, Sink::Inherit, Sink::Inherit)
                .await
                .success
        }
        Step::ProbePlugin(i) => ladder
            .plugin_paths
            .get(i)
            .is_some_and(|path| is_executable_file(path)),
        Step::PluginUp(i) => {
            let Some(program) = ladder.plugin_paths.get(i) else {
                return false;
            };
            // Allowlist at the call site: the only variable-program spawn in
            // this module, and it can only ever launch a rung 3c candidate.
            if !is_allowed_plugin_program(program, &ladder.plugin_paths) {
                return false;
            }
            let args = compose_up_args(false, &settings.compose_file);
            let mut cmd = Command::new(program);
            cmd.args(&args).env("PATH", &settings.path);
            let cmd = with_compose_env(cmd, creds.as_ref());
            run_bounded(cmd, settings.call_timeout, Sink::Inherit, Sink::Inherit)
                .await
                .success
        }
        Step::DockerRun => {
            let Some(pg) = creds.as_ref() else {
                return false;
            };
            let http_bind = resolve_http_bind(
                settings.http_bind_env.as_deref(),
                read_dotenv(settings).as_deref(),
            );
            let args = docker_run_args(&http_bind);
            let mut cmd = Command::new("docker");
            cmd.args(&args)
                .env("PATH", &settings.path)
                .env("QDB_PG_USER", pg.user.expose_secret())
                .env("QDB_PG_PASSWORD", pg.password.expose_secret());
            run_bounded(cmd, settings.run_timeout, Sink::Null, Sink::Inherit)
                .await
                .success
        }
        Step::Done(_) => false,
    }
}

/// `docker <args>` with both streams discarded.
async fn quiet(settings: &Settings, args: &[&str], limit: Duration) -> bool {
    run_bounded(docker(settings, args), limit, Sink::Null, Sink::Null)
        .await
        .success
}

/// The compose project's `.env`, when the environment did not set the bind.
fn read_dotenv(settings: &Settings) -> Option<String> {
    if settings
        .http_bind_env
        .as_deref()
        .is_some_and(|v| !v.is_empty())
    {
        return None;
    }
    let dir = Path::new(&settings.compose_file)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    std::fs::read_to_string(dir.join(".env")).ok()
}

/// One journal line, prefixed as the script's `log()` did. A closed stdout
/// must never stop the self-heal, so a write error is ignored.
fn say(line: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    if out
        .write_all(format!("ensure-questdb: {line}\n").as_bytes())
        .is_ok()
    {
        out.flush().unwrap_or(());
    }
}

/// Runs the ladder and returns the process exit code (0 up, 1 failed).
///
/// Called from `main.rs` for `tickvault ensure-questdb`, after the rustls
/// provider is installed (the SSM client needs TLS) and before config loads.
// TEST-EXEMPT: thin I/O loop over docker and SSM; every decision it takes comes from Ladder, covered by the ladder_* tests, and run_bounded has its own tests.
pub async fn run_ensure_questdb() -> i32 {
    let settings = Settings::from_lookup(|key| std::env::var(key).ok());
    let ssm_env = tickvault_core::auth::secret_manager::resolve_environment()
        .unwrap_or_else(|_| "<invalid TV_ENVIRONMENT>".to_string());
    let mut ladder = Ladder::new(
        compose_plugin_candidates(settings.home.as_deref()),
        ssm_env,
        settings.pull_timeout,
    );
    let mut creds: Option<PgCreds> = None;
    let mut step = Step::ProbeRunning;
    for _ in 0..MAX_STEPS {
        if let Step::Done(verdict) = step {
            return verdict.exit_code();
        }
        if let Some(line) = ladder.announce(step) {
            say(&line);
        }
        let ok = execute(step, &settings, &ladder, &mut creds).await;
        if let Some(pause) = pause_after(step, ok) {
            tokio::time::sleep(pause).await;
        }
        let transition = ladder.transition(step, ok);
        if let Some(line) = transition.say {
            say(&line);
        }
        step = transition.next;
    }
    match step {
        Step::Done(verdict) => verdict.exit_code(),
        _ => {
            say(&format!("FAILED to bring up {SVC} by any method"));
            Verdict::Failed.exit_code()
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ladder(plugins: usize) -> Ladder {
        let paths = (0..plugins)
            .map(|i| format!("/p{i}/docker-compose"))
            .collect();
        Ladder::new(paths, "prod".to_string(), Duration::from_secs(300))
    }

    fn walk(l: &mut Ladder, outcomes: &[(Step, bool)]) -> Vec<Transition> {
        outcomes
            .iter()
            .map(|(s, ok)| l.transition(*s, *ok))
            .collect()
    }

    #[test]
    fn test_is_ensure_questdb_invocation_positional_only() {
        assert!(is_ensure_questdb_invocation([
            "tickvault",
            "ensure-questdb"
        ]));
        assert!(is_ensure_questdb_invocation([
            "/opt/tickvault/bin/tickvault",
            "ensure-questdb",
            "x"
        ]));
        assert!(!is_ensure_questdb_invocation(["tickvault"]));
        assert!(!is_ensure_questdb_invocation([
            "tickvault",
            "--check-trading-day"
        ]));
        assert!(!is_ensure_questdb_invocation([
            "tickvault",
            "x",
            "ensure-questdb"
        ]));
        assert!(!is_ensure_questdb_invocation([
            "tickvault",
            "ensure-questdb.sh"
        ]));
        assert!(!is_ensure_questdb_invocation(Vec::<String>::new()));
    }

    #[test]
    fn test_run_ensure_questdb_exit_codes_match_the_script() {
        assert_eq!(Verdict::Up.exit_code(), 0);
        assert_eq!(Verdict::Failed.exit_code(), 1);
    }

    #[test]
    fn ladder_running_container_is_a_no_op() {
        let mut l = ladder(4);
        let t = l.transition(Step::ProbeRunning, true);
        assert_eq!(t.next, Step::Done(Verdict::Up));
        assert_eq!(t.say.as_deref(), Some("tv-questdb already running"));
    }

    #[test]
    fn ladder_stopped_container_is_started_without_creds() {
        let mut l = ladder(4);
        let t = walk(
            &mut l,
            &[(Step::ProbeRunning, false), (Step::ProbeExists, true)],
        );
        assert_eq!(t[0].next, Step::ProbeExists);
        assert_eq!(t[1].next, Step::StartExisting);
        let started = l.transition(Step::StartExisting, true);
        assert_eq!(started.next, Step::Done(Verdict::Up));
        assert_eq!(
            started.say.as_deref(),
            Some("started existing stopped tv-questdb")
        );
    }

    #[test]
    fn ladder_failed_start_falls_through_to_recreate() {
        let mut l = ladder(4);
        let t = l.transition(Step::StartExisting, false);
        assert_eq!(t.next, Step::FetchCreds);
        assert!(
            t.say
                .as_deref()
                .is_some_and(|s| s.starts_with("docker start failed"))
        );
    }

    #[test]
    fn ladder_missing_container_goes_straight_to_creds() {
        let mut l = ladder(4);
        assert_eq!(
            l.transition(Step::ProbeExists, false).next,
            Step::FetchCreds
        );
    }

    #[test]
    fn ladder_creds_outcome_is_remembered_and_warned() {
        let mut l = ladder(0);
        let ok = l.transition(Step::FetchCreds, true);
        assert_eq!(
            ok,
            Transition {
                next: Step::ProbeImage,
                say: None
            }
        );
        assert!(l.creds_present);
        let bad = l.transition(Step::FetchCreds, false);
        assert_eq!(bad.next, Step::ProbeImage);
        assert!(!l.creds_present);
        assert_eq!(
            bad.say.as_deref(),
            Some(
                "WARN: could not read QuestDB PG creds from SSM (/tickvault/prod/questdb/*); recreate may fail"
            )
        );
    }

    #[test]
    fn ladder_present_image_skips_the_pull() {
        let mut l = ladder(4);
        let t = l.transition(Step::ProbeImage, true);
        assert_eq!(t.next, Step::ProbeComposeV2);
        assert!(
            t.say
                .as_deref()
                .is_some_and(|s| s.contains("already present - skipping pull"))
        );
    }

    #[test]
    fn ladder_pull_retries_three_times_then_continues() {
        let mut l = ladder(4);
        assert_eq!(l.transition(Step::ProbeImage, false).next, Step::Pull(1));
        assert_eq!(l.transition(Step::Pull(1), false).next, Step::Pull(2));
        assert_eq!(l.transition(Step::Pull(2), false).next, Step::Pull(3));
        let gave_up = l.transition(Step::Pull(3), false);
        assert_eq!(gave_up.next, Step::ProbeComposeV2);
        assert!(
            gave_up
                .say
                .as_deref()
                .is_some_and(|s| s.contains("after 3 bounded attempts"))
        );
        for attempt in 1..=3 {
            let pulled = l.transition(Step::Pull(attempt), true);
            assert_eq!(pulled.next, Step::ProbeComposeV2);
            assert!(
                pulled
                    .say
                    .as_deref()
                    .is_some_and(|s| s.ends_with(" pulled"))
            );
        }
    }

    #[test]
    fn ladder_pull_announce_names_attempt_and_bound() {
        let l = ladder(4);
        assert_eq!(
            l.announce(Step::Pull(2)),
            Some(format!("pulling {IMAGE} (attempt 2/3, bounded 300s)"))
        );
        assert_eq!(
            l.announce(Step::DockerRun).as_deref(),
            Some("no compose found — recreating tv-questdb via 'docker run'")
        );
        assert_eq!(l.announce(Step::ProbeRunning), None);
        assert_eq!(l.announce(Step::ComposeV2Up), None);
    }

    #[test]
    fn ladder_pause_follows_every_failed_pull_only() {
        for attempt in 1..=3 {
            assert_eq!(
                pause_after(Step::Pull(attempt), false),
                Some(PULL_RETRY_PAUSE)
            );
            assert_eq!(pause_after(Step::Pull(attempt), true), None);
        }
        assert_eq!(pause_after(Step::ComposeV2Up, false), None);
        assert_eq!(pause_after(Step::DockerRun, false), None);
        assert_eq!(PULL_RETRY_PAUSE.as_secs(), 10);
    }

    #[test]
    fn ladder_compose_v2_then_v1_then_plugins() {
        let mut l = ladder(4);
        assert_eq!(
            l.transition(Step::ProbeComposeV2, true).next,
            Step::ComposeV2Up
        );
        assert_eq!(
            l.transition(Step::ComposeV2Up, true).say.as_deref(),
            Some("recreated via 'docker compose' (v2)")
        );
        assert_eq!(
            l.transition(Step::ComposeV2Up, false).next,
            Step::ProbeComposeV1
        );
        assert_eq!(
            l.transition(Step::ProbeComposeV2, false).next,
            Step::ProbeComposeV1
        );
        assert_eq!(
            l.transition(Step::ProbeComposeV1, true).next,
            Step::ComposeV1Up
        );
        assert_eq!(
            l.transition(Step::ComposeV1Up, true).say.as_deref(),
            Some("recreated via 'docker-compose' (v1)")
        );
        assert_eq!(
            l.transition(Step::ComposeV1Up, false).next,
            Step::ProbePlugin(0)
        );
        assert_eq!(
            l.transition(Step::ProbeComposeV1, false).next,
            Step::ProbePlugin(0)
        );
    }

    #[test]
    fn ladder_plugins_are_probed_in_order() {
        let mut l = ladder(4);
        assert_eq!(
            l.transition(Step::ProbePlugin(0), false).next,
            Step::ProbePlugin(1)
        );
        assert_eq!(
            l.transition(Step::ProbePlugin(1), true).next,
            Step::PluginUp(1)
        );
        assert_eq!(
            l.transition(Step::PluginUp(1), false).next,
            Step::ProbePlugin(2)
        );
        let up = l.transition(Step::PluginUp(2), true);
        assert_eq!(up.next, Step::Done(Verdict::Up));
        assert_eq!(
            up.say.as_deref(),
            Some("recreated via compose plugin at /p2/docker-compose")
        );
    }

    #[test]
    fn ladder_without_creds_refuses_docker_run() {
        let mut l = ladder(4);
        l.transition(Step::FetchCreds, false);
        let t = l.transition(Step::ProbePlugin(3), false);
        assert_eq!(t.next, Step::Done(Verdict::Failed));
        assert!(
            t.say
                .as_deref()
                .is_some_and(|s| s.starts_with("FAILED: no compose available AND no PG creds"))
        );
        let t = l.transition(Step::PluginUp(3), false);
        assert_eq!(t.next, Step::Done(Verdict::Failed));
    }

    #[test]
    fn ladder_with_creds_falls_back_to_docker_run() {
        let mut l = ladder(4);
        l.transition(Step::FetchCreds, true);
        assert_eq!(
            l.transition(Step::ProbePlugin(3), false).next,
            Step::DockerRun
        );
        // No plugin candidates at all: straight from v1 to the gate.
        let mut none = ladder(0);
        none.transition(Step::FetchCreds, true);
        assert_eq!(
            none.transition(Step::ProbeComposeV1, false).next,
            Step::DockerRun
        );
    }

    #[test]
    fn ladder_docker_run_verdicts() {
        let mut l = ladder(4);
        let up = l.transition(Step::DockerRun, true);
        assert_eq!(up.next, Step::Done(Verdict::Up));
        assert_eq!(up.say.as_deref(), Some("recreated via 'docker run'"));
        let failed = l.transition(Step::DockerRun, false);
        assert_eq!(failed.next, Step::Done(Verdict::Failed));
        assert_eq!(
            failed.say.as_deref(),
            Some("FAILED to bring up tv-questdb by any method")
        );
    }

    #[test]
    fn ladder_done_is_absorbing() {
        let mut l = ladder(4);
        for v in [Verdict::Up, Verdict::Failed] {
            for ok in [true, false] {
                assert_eq!(l.transition(Step::Done(v), ok).next, Step::Done(v));
            }
        }
    }

    /// Depth-first walk over EVERY success/failure permutation from the first
    /// rung: each path ends in a verdict within `MAX_STEPS`, an `Up` verdict is
    /// reached only through a step that succeeded, and `docker run` is never
    /// reached without credentials.
    #[test]
    fn ladder_terminates_on_every_outcome_permutation() {
        fn dfs(l: &Ladder, step: Step, depth: usize, leaves: &mut usize, deepest: &mut usize) {
            assert!(
                depth <= MAX_STEPS,
                "ladder did not terminate within {MAX_STEPS} steps"
            );
            if let Step::Done(_) = step {
                *leaves += 1;
                *deepest = (*deepest).max(depth);
                return;
            }
            if step == Step::DockerRun {
                assert!(l.creds_present, "docker run reached without credentials");
            }
            for ok in [true, false] {
                let mut next_l = l.clone();
                let t = next_l.transition(step, ok);
                if t.next == Step::Done(Verdict::Up) {
                    assert!(ok, "{step:?} failed yet the ladder reported up");
                }
                dfs(&next_l, t.next, depth + 1, leaves, deepest);
            }
        }
        for plugins in [0, 1, 4] {
            let l = ladder(plugins);
            let mut leaves = 0;
            let mut deepest = 0;
            dfs(&l, Step::ProbeRunning, 0, &mut leaves, &mut deepest);
            assert!(leaves > 10, "walk is vacuous: {leaves} leaves");
            if plugins == 4 {
                // 2 + start + creds + image + 3 pulls + v2(2) + v1(2) + 4×2 + run = 21.
                assert_eq!(deepest, 21);
            }
        }
    }

    #[test]
    fn timeouts_follow_the_script_overrides() {
        let secs = |raw: Option<&str>, d: u64| timeout_from(raw, d).as_secs_f64();
        assert_eq!(secs(None, 90), 90.0);
        assert_eq!(secs(Some(""), 90), 90.0);
        assert_eq!(secs(Some("120"), 90), 120.0);
        assert_eq!(secs(Some(" 2.5 "), 90), 2.5);
        assert_eq!(secs(Some("0"), 300), 300.0);
        assert_eq!(secs(Some("-5"), 300), 300.0);
        assert_eq!(secs(Some("abc"), 300), 300.0);
        assert_eq!(secs(Some("inf"), 300), 300.0);
        assert_eq!(secs(Some("1e300"), 300), 300.0);
    }

    #[test]
    fn settings_read_every_override() {
        let env = |k: &str| match k {
            "TV_COMPOSE_FILE" => Some("/x/compose.yml".to_string()),
            "DOCKER_CALL_TIMEOUT_SECS" => Some("5".to_string()),
            "DOCKER_PULL_TIMEOUT_SECS" => Some("6".to_string()),
            "DOCKER_RUN_TIMEOUT_SECS" => Some("7".to_string()),
            "PATH" => Some("/opt/bin".to_string()),
            "HOME" => Some("/root".to_string()),
            "TV_QDB_HTTP_BIND" => Some("0.0.0.0".to_string()),
            _ => None,
        };
        let s = Settings::from_lookup(env);
        assert_eq!(s.compose_file, "/x/compose.yml");
        assert_eq!(s.call_timeout.as_secs(), 5);
        assert_eq!(s.pull_timeout.as_secs(), 6);
        assert_eq!(s.run_timeout.as_secs(), 7);
        assert_eq!(s.path, format!("{PATH_PREFIX}:/opt/bin"));
        assert_eq!(s.home.as_deref(), Some("/root"));
        assert_eq!(s.http_bind_env.as_deref(), Some("0.0.0.0"));

        let d = Settings::from_lookup(|_| None);
        assert_eq!(d.compose_file, DEFAULT_COMPOSE_FILE);
        assert_eq!(d.call_timeout.as_secs(), 90);
        assert_eq!(d.pull_timeout.as_secs(), 300);
        assert_eq!(d.run_timeout.as_secs(), 300);
        assert_eq!(d.path, PATH_PREFIX);
        assert_eq!(d.home, None);
    }

    #[test]
    fn effective_path_prepends_the_standard_dirs() {
        assert_eq!(
            effective_path(Some("/a:/b")),
            format!("{PATH_PREFIX}:/a:/b")
        );
        assert_eq!(effective_path(None), PATH_PREFIX);
        assert!(PATH_PREFIX.starts_with("/usr/local/sbin:/usr/local/bin:"));
    }

    #[test]
    fn plugin_candidates_are_the_scripts_four_paths() {
        let c = compose_plugin_candidates(Some("/home/ops"));
        assert_eq!(
            c,
            vec![
                "/usr/local/lib/docker/cli-plugins/docker-compose".to_string(),
                "/usr/libexec/docker/cli-plugins/docker-compose".to_string(),
                "/usr/lib/docker/cli-plugins/docker-compose".to_string(),
                "/home/ops/.docker/cli-plugins/docker-compose".to_string(),
            ]
        );
        for home in [None, Some("")] {
            assert_eq!(
                compose_plugin_candidates(home).last().map(String::as_str),
                Some("/home/ec2-user/.docker/cli-plugins/docker-compose")
            );
        }
        assert_eq!(
            compose_plugin_candidates(Some("/root/"))
                .last()
                .map(String::as_str),
            Some("/root/.docker/cli-plugins/docker-compose")
        );
    }

    #[test]
    fn plugin_spawn_allowlist_admits_only_candidates() {
        let c = compose_plugin_candidates(Some("/root"));
        for p in &c {
            assert!(is_allowed_plugin_program(p, &c), "{p}");
        }
        assert!(!is_allowed_plugin_program("docker-compose", &c));
        assert!(!is_allowed_plugin_program("/usr/bin/sh", &c));
        assert!(!is_allowed_plugin_program("/tmp/docker-compose", &c));
        let planted = vec!["/x/../bin/docker-compose".to_string()];
        assert!(!is_allowed_plugin_program(&planted[0], &planted));
        let relative = vec!["rel/docker-compose".to_string()];
        assert!(!is_allowed_plugin_program(&relative[0], &relative));
    }

    #[test]
    fn running_probe_matches_command_substitution() {
        assert!(running_probe_says_true(b"true\n"));
        assert!(running_probe_says_true(b"true"));
        assert!(running_probe_says_true(b"true\n\n"));
        assert!(!running_probe_says_true(b"false\n"));
        assert!(!running_probe_says_true(b""));
        assert!(!running_probe_says_true(b" true\n"));
        assert!(!running_probe_says_true(b"true\r\n"));
        assert!(!running_probe_says_true(b"true\ntrue\n"));
    }

    #[test]
    fn dotenv_bind_is_the_last_matching_line_verbatim() {
        assert_eq!(
            http_bind_from_dotenv("A=1\nTV_QDB_HTTP_BIND=0.0.0.0\n"),
            Some("0.0.0.0".into())
        );
        assert_eq!(
            http_bind_from_dotenv("TV_QDB_HTTP_BIND=1.1.1.1\nTV_QDB_HTTP_BIND=0.0.0.0"),
            Some("0.0.0.0".into())
        );
        assert_eq!(
            http_bind_from_dotenv("# TV_QDB_HTTP_BIND=0.0.0.0\n X=1"),
            None
        );
        assert_eq!(http_bind_from_dotenv(" TV_QDB_HTTP_BIND=0.0.0.0"), None);
        assert_eq!(http_bind_from_dotenv(""), None);
    }

    #[test]
    fn http_bind_prefers_env_then_dotenv_then_default() {
        let dotenv = Some("TV_QDB_HTTP_BIND=0.0.0.0\n");
        assert_eq!(resolve_http_bind(Some("10.0.0.5"), dotenv), "10.0.0.5");
        assert_eq!(resolve_http_bind(Some(""), dotenv), "0.0.0.0");
        assert_eq!(resolve_http_bind(None, dotenv), "0.0.0.0");
        assert_eq!(
            resolve_http_bind(None, Some("TV_QDB_HTTP_BIND=\n")),
            DEFAULT_HTTP_BIND
        );
        assert_eq!(resolve_http_bind(None, None), DEFAULT_HTTP_BIND);
    }

    #[test]
    fn compose_up_args_keep_the_r1_ordering() {
        assert_eq!(
            compose_up_args(true, "/c.yml"),
            ["compose", "-f", "/c.yml", "up", "-d", "tv-questdb"]
        );
        assert_eq!(
            compose_up_args(false, "/c.yml"),
            ["-f", "/c.yml", "up", "-d", "tv-questdb"]
        );
    }

    #[test]
    fn docker_run_args_mirror_the_compose_spec_and_hide_secrets() {
        let args = docker_run_args("0.0.0.0");
        let joined = args.join(" ");
        assert_eq!(
            joined,
            format!(
                "run -d --name tv-questdb --hostname tv-questdb --restart unless-stopped \
                 -p 0.0.0.0:9000:9000 -p 8812:8812 -p 9009:9009 -p 127.0.0.1:9003:9003 \
                 -v tv-questdb-data:/var/lib/questdb --shm-size 512m --memory 2g \
                 -e QDB_TELEMETRY_ENABLED=false -e QDB_METRICS_ENABLED=TRUE \
                 -e QDB_PG_USER -e QDB_PG_PASSWORD {IMAGE}"
            )
        );
        // Value-less `-e NAME`: no `=` after either credential name.
        assert!(!joined.contains("QDB_PG_USER="));
        assert!(!joined.contains("QDB_PG_PASSWORD=")); // secret-scan-ignore: asserts no value is passed
        assert_eq!(args.last().map(String::as_str), Some(IMAGE));
    }

    #[test]
    fn image_matches_the_compose_file_pin() {
        let compose = include_str!("../../../deploy/docker/docker-compose.yml");
        assert!(
            compose.contains(&format!("image: {IMAGE}")),
            "ensure-questdb's image drifted from deploy/docker/docker-compose.yml"
        );
        assert!(IMAGE.contains("@sha256:"));
    }

    #[test]
    fn find_on_path_follows_command_v() {
        assert!(find_on_path("sh", "/nonexistent-dir:/bin:/usr/bin").is_some());
        assert_eq!(
            find_on_path("definitely-not-a-binary-d6d", "/bin:/usr/bin"),
            None
        );
        assert_eq!(find_on_path("sh", ""), find_on_path("sh", "."));
    }

    #[tokio::test]
    async fn run_bounded_reports_success_and_failure() {
        let ok = run_bounded(
            Command::new("/usr/bin/true"),
            Duration::from_secs(10),
            Sink::Null,
            Sink::Null,
        )
        .await;
        assert!(ok.success);
        let bad = run_bounded(
            {
                let mut c = Command::new("sh");
                c.args(["-c", "exit 1"]);
                c
            },
            Duration::from_secs(10),
            Sink::Null,
            Sink::Null,
        )
        .await;
        assert!(!bad.success);
        let missing = run_bounded(
            {
                // A spawn that fails before the program runs: the working
                // directory does not exist.
                let mut c = Command::new("/usr/bin/true");
                c.current_dir("/definitely-not-a-dir-d6d");
                c
            },
            Duration::from_secs(10),
            Sink::Null,
            Sink::Null,
        )
        .await;
        assert!(!missing.success);
    }

    #[tokio::test]
    async fn run_bounded_captures_stdout_for_the_running_probe() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo true"]);
        let out = run_bounded(cmd, Duration::from_secs(10), Sink::Capture, Sink::Null).await;
        assert!(out.success);
        assert!(running_probe_says_true(&out.stdout));
    }

    #[tokio::test]
    async fn run_bounded_kills_a_child_that_outlives_its_bound() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec sleep 30"]);
        let started = std::time::Instant::now();
        let out = run_bounded(cmd, Duration::from_millis(200), Sink::Null, Sink::Null).await;
        assert!(!out.success, "a timed-out call must read as failed");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the bound did not hold"
        );
    }
}
