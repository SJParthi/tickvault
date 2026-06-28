//! Groww Python-sidecar auto-launcher + supervisor (operator lock §32 +
//! "no manual commands" directive 2026-06-19).
//!
//! Operator verbatim 2026-06-19: *"i never ever want to install or run any
//! commands manually everything should be always entirely automated and
//! runnable"*. This module is that automation for the **Python-now** half of
//! the "Both — Python now, Rust later" Groww path: when `feeds.groww_enabled`
//! is set, tickvault itself
//!
//! 1. **auto-provisions** an isolated Python virtual-env under
//!    `data/groww/venv` and `pip install`s `growwapi` + `pyotp` into it ONCE
//!    (idempotent — skipped if the venv python already exists), so the
//!    operator never types `pip install`. The system interpreter used to
//!    BUILD the venv is **auto-discovered** ([`discover_system_python`]) across
//!    every place a normal Mac/AWS host keeps it (Xcode CLT `/usr/bin/python3`,
//!    Homebrew `/opt/homebrew` + `/usr/local`, versioned `python3.x`, plain
//!    `python3` on PATH) — so "click Run" self-provisions with no manual
//!    Python install. (Auto-INSTALLING the OS interpreter is out of scope:
//!    `brew` is BANNED per CLAUDE.md and an OS package install needs `sudo` —
//!    neither is one-click-safe; every normal Mac/AWS host already ships one,
//!    which discovery finds.);
//! 2. **fetches** the Groww credentials from SSM (the SAME
//!    `/tickvault/<env>/groww/api-key` + `/totp-secret` the native auth path
//!    uses) and injects them as env vars into the child — never logged;
//! 3. **spawns** `scripts/groww-sidecar/groww_sidecar.py` from that venv,
//!    which appends capture-at-receipt NDJSON ticks the [`crate::groww_bridge`]
//!    consumer tails;
//! 4. **supervises** the child: on crash/exit it restarts with exponential
//!    backoff (mirrors the WS pool supervisor pattern WS-GAP-05); on a runtime
//!    `groww_enabled=false` toggle (via the feed-toggle API) it stops the
//!    child and idles until re-enabled — true two-way control.
//!
//! **Default OFF + isolated:** nothing here runs unless `feeds.groww_enabled`.
//! It spawns ONLY the Groww sidecar process and touches NO Dhan path. The
//! supervise loop is a TEST-EXEMPT process/IO driver; the pure, fully
//! unit-tested primitives are the restart-backoff curve, the venv-python path
//! resolver, the provisioning gate, and the command/arg builders.
//!
//! # Security (3-agent review 2026-06-19 — 0 critical / 0 high / 2 medium / 1 low)
//!
//! - Credentials are injected via [`Command::env`], NEVER argv — so they are
//!   not visible in a `ps`/argv listing and are never logged (`expose_secret`
//!   is called only at the injection point, never stored or re-logged).
//!   Residual MEDIUM: a same-UID process could read `/proc/<pid>/environ`. On
//!   this single-app host the only same-UID processes are ours; tightening to
//!   a stdin/pipe channel (would require the sidecar to stop reading
//!   `os.environ`) is a documented follow-up, not a blocker.
//! - `pip install -r requirements.txt` can run package `setup.py` at cold boot
//!   (MEDIUM). Mitigated: the manifest is version-controlled + pinned and the
//!   venv is isolated under `data/`. Inherent to the operator-chosen `growwapi`
//!   SDK path (lock §32).
//! - `system_python` / all command parts come from compile-time constants, not
//!   external input — no command-injection surface. LOW: if `GrowwSidecarOptions`
//!   is ever wired to a config/API source, the executable + paths MUST stay
//!   allowlisted.
//! - The bare-name [`PYTHON_CANDIDATES`] (`python3`, `python3.x`, `python`) are
//!   `$PATH`-resolved at probe time, so a binary named `python3` placed ahead of
//!   the real one on `$PATH` would run during discovery (security-review HIGH
//!   2026-06-21). This is INHERENT to finding `python3` at all (the pre-discovery
//!   code already spawned bare `python3`), and on this single-app / single-UID
//!   host a writable-`$PATH` attacker already has code execution — so it is
//!   accepted defense-in-depth, not a new hole. The three ABSOLUTE candidates
//!   (`/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`) are PATH-independent
//!   anchors; eliminating bare names entirely would break discovery on hosts that
//!   keep `python3` elsewhere. The probe runs `-c "import venv"` (capability
//!   check), not arbitrary input.
//!
//! Honest envelope (operator §F): this is the Python validation path of lock
//! §32 — capture-at-receipt durability (the sidecar fsyncs each tick the
//! instant the callback fires) feeding the existing Rust ring→spill→DLQ chain.
//! O(1) is NOT claimed on the Python hop (GIL/GC jitter); it IS preserved on
//! the Rust consumer/aggregator. The native-Rust at-socket connector remains
//! the production option (lock §32.2).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use secrecy::ExposeSecret;
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{error, info, warn};

use tickvault_api::feed_state::{Feed, FeedRuntimeState};
use tickvault_core::auth::secret_manager::{
    build_ssm_path, fetch_groww_credentials, resolve_environment,
};

/// System Python used ONLY to create the isolated venv (never to run the
/// sidecar — the sidecar runs from the venv's own python). This is the FIRST
/// candidate tried; [`discover_system_python`] falls through [`PYTHON_CANDIDATES`]
/// if it is absent.
pub const SYSTEM_PYTHON_DEFAULT: &str = "python3";

/// Candidate system-Python executables tried (in order) when the preferred
/// interpreter is absent, so "click Run" self-provisions on any normal host with
/// ZERO manual install. Covers a clean macOS (`/usr/bin/python3` from Xcode CLT,
/// Homebrew `/opt/homebrew` on Apple Silicon + `/usr/local` on Intel) and a clean
/// AWS Linux host (`python3` on PATH + versioned fallbacks). We DISCOVER, never
/// auto-install: `brew` is BANNED (CLAUDE.md) and OS package install needs `sudo`.
pub const PYTHON_CANDIDATES: &[&str] = &[
    "python3",
    "python3.13",
    "python3.12",
    "python3.11",
    "python3.10",
    "/opt/homebrew/bin/python3",
    "/usr/local/bin/python3",
    "/usr/bin/python3",
    "python",
];

/// Pure interpreter picker: returns `preferred` if it works, else the first
/// [`PYTHON_CANDIDATES`] entry for which `probe` reports a working interpreter,
/// else `preferred` (so the caller's existing "is python available?" error path
/// fires when NOTHING works). O(candidates), no I/O — `probe` is injected so the
/// selection logic is fully unit-tested.
#[must_use]
pub fn pick_system_python<F: Fn(&str) -> bool>(preferred: &str, probe: F) -> String {
    if probe(preferred) {
        return preferred.to_string();
    }
    for candidate in PYTHON_CANDIDATES {
        if *candidate != preferred && probe(candidate) {
            return (*candidate).to_string();
        }
    }
    preferred.to_string()
}

/// Default isolated virtual-env directory (kept under `data/` like the rest of
/// the runtime state; created once, reused every boot).
pub const GROWW_SIDECAR_VENV_DEFAULT: &str = "data/groww/venv";

/// Default path of the Python sidecar producer script.
pub const GROWW_SIDECAR_SCRIPT_DEFAULT: &str = "scripts/groww-sidecar/groww_sidecar.py";

/// Default path of the sidecar's pinned dependency manifest.
pub const GROWW_SIDECAR_REQUIREMENTS_DEFAULT: &str = "scripts/groww-sidecar/requirements.txt";

/// Base unit of the restart backoff curve (first failure waits this long).
pub const SIDECAR_RESTART_BASE_SECS: u64 = 2;

/// Cap of the restart backoff curve — never wait longer than this between
/// restarts so a recovered sidecar resumes promptly.
pub const SIDECAR_RESTART_MAX_SECS: u64 = 60;

/// Poll cadence (seconds) while the Groww feed is disabled, and while watching a
/// running child for a runtime disable toggle.
pub const SIDECAR_DISABLE_POLL_SECS: u64 = 2;

/// Poll cadence while the Groww feed is disabled, and while watching a running
/// child for a runtime disable toggle.
pub const SIDECAR_DISABLE_POLL: Duration = Duration::from_secs(SIDECAR_DISABLE_POLL_SECS);

/// Hard ceiling (seconds) on the one-time venv-create + `pip install`
/// provisioning step.
pub const SIDECAR_PROVISION_TIMEOUT_SECS: u64 = 300;

/// Hard ceiling on the one-time venv-create + `pip install` provisioning step,
/// so a hung PyPI connection can never block the supervisor forever (it instead
/// fails, backs off, and retries — hostile-review MEDIUM 2026-06-19).
pub const SIDECAR_PROVISION_TIMEOUT: Duration = Duration::from_secs(SIDECAR_PROVISION_TIMEOUT_SECS);

/// All paths the supervisor needs. Constructed with [`Default`] in the boot
/// path; overridable in tests.
#[derive(Debug, Clone)]
pub struct GrowwSidecarOptions {
    /// System python used to build the venv.
    pub system_python: String,
    /// Isolated venv directory.
    pub venv_dir: PathBuf,
    /// Sidecar producer script.
    pub script_path: PathBuf,
    /// Pinned dependency manifest.
    pub requirements_path: PathBuf,
    /// The capture-at-receipt NDJSON file the sidecar appends and the bridge
    /// tails. Passed to the child as `GROWW_TICK_FILE`.
    pub tick_file: PathBuf,
}

impl Default for GrowwSidecarOptions {
    fn default() -> Self {
        Self {
            system_python: SYSTEM_PYTHON_DEFAULT.to_string(),
            venv_dir: PathBuf::from(GROWW_SIDECAR_VENV_DEFAULT),
            script_path: PathBuf::from(GROWW_SIDECAR_SCRIPT_DEFAULT),
            requirements_path: PathBuf::from(GROWW_SIDECAR_REQUIREMENTS_DEFAULT),
            tick_file: PathBuf::from(crate::groww_bridge::GROWW_TICK_FILE_DEFAULT),
        }
    }
}

/// Exponential restart backoff, capped. Failure 0 → no wait (first launch is
/// immediate); failure N≥1 → `min(BASE * 2^(N-1), MAX)`.
///
/// Pure + O(1); mirrors the WS reconnect backoff shape so a chronically
/// crashing sidecar does not hot-loop the host.
#[must_use]
pub fn sidecar_restart_backoff(consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return Duration::ZERO;
    }
    let shift = consecutive_failures.saturating_sub(1).min(30);
    let scaled = SIDECAR_RESTART_BASE_SECS.saturating_mul(1u64 << shift);
    Duration::from_secs(scaled.min(SIDECAR_RESTART_MAX_SECS))
}

/// Resolve the venv's own python interpreter (`<venv>/bin/python3`). Pure path
/// join — no I/O.
#[must_use]
pub fn venv_python_path(venv_dir: &Path) -> PathBuf {
    venv_dir.join("bin").join("python3")
}

/// True when the venv already has a python interpreter — the idempotency gate
/// that makes provisioning a one-time, zero-touch step.
#[must_use]
pub fn venv_is_provisioned(venv_dir: &Path) -> bool {
    venv_python_path(venv_dir).is_file()
}

/// Build the `python3 -m venv <dir>` command parts (program, args). Pure.
#[must_use]
pub fn build_venv_create_command(system_python: &str, venv_dir: &Path) -> (String, Vec<String>) {
    (
        system_python.to_string(),
        vec![
            "-m".to_string(),
            "venv".to_string(),
            venv_dir.to_string_lossy().into_owned(),
        ],
    )
}

/// Build the `<venv-python> -m pip install --quiet -r <requirements>` command
/// parts (program, args). Pure.
#[must_use]
pub fn build_pip_install_command(
    venv_python: &Path,
    requirements_path: &Path,
) -> (String, Vec<String>) {
    (
        venv_python.to_string_lossy().into_owned(),
        vec![
            "-m".to_string(),
            "pip".to_string(),
            "install".to_string(),
            "--quiet".to_string(),
            "--disable-pip-version-check".to_string(),
            "-r".to_string(),
            requirements_path.to_string_lossy().into_owned(),
        ],
    )
}

/// Build the `<venv-python> <script>` sidecar run command parts (program,
/// args). Pure. Credentials + tick-file are injected as ENV at spawn, never as
/// args (so they never appear in a process listing / log).
#[must_use]
pub fn build_sidecar_run_command(venv_python: &Path, script_path: &Path) -> (String, Vec<String>) {
    (
        venv_python.to_string_lossy().into_owned(),
        vec![script_path.to_string_lossy().into_owned()],
    )
}

/// Probe whether `candidate` is a Python that can ACTUALLY build the venv —
/// `candidate -c "import venv"` exits 0. This is the exact capability
/// `ensure_python_env` needs (`-m venv`), so it is stronger than a bare
/// `--version` check: it rejects a broken PATH shim that prints a version but
/// can't create a venv (hostile-review LOW 2026-06-21), and it rejects Python 2
/// (no `venv` module) so the trailing `python` candidate can never be mis-picked.
/// One cheap child spawn.
// TEST-EXEMPT: spawns a child process to probe the host's interpreter capability; the selection logic that consumes these facts (pick_system_python) is unit-tested with an injected probe.
async fn python_works(candidate: &str) -> bool {
    matches!(
        Command::new(candidate).args(["-c", "import venv"]).output().await,
        Ok(out) if out.status.success()
    )
}

/// Discover a usable system Python, preferring `preferred`. Gathers which
/// candidates actually run (`--version`), then delegates the CHOICE to the pure,
/// unit-tested [`pick_system_python`] — so the I/O (probing) and the decision
/// (ordering/fallback) stay cleanly separated.
// TEST-EXEMPT: drives `--version` child spawns to learn which interpreters exist on this host; the ordering/fallback decision it returns is the unit-tested pure pick_system_python.
async fn discover_system_python(preferred: &str) -> String {
    let mut working: std::collections::HashSet<String> = std::collections::HashSet::new();
    if python_works(preferred).await {
        working.insert(preferred.to_string());
    }
    for candidate in PYTHON_CANDIDATES {
        if !working.contains(*candidate) && python_works(candidate).await {
            working.insert((*candidate).to_string());
        }
    }
    pick_system_python(preferred, |c| working.contains(c))
}

/// Provision the isolated venv + dependencies if not already present.
/// Idempotent: returns early when [`venv_is_provisioned`] is true.
// TEST-EXEMPT: spawns `python -m venv` + `pip install` child processes + filesystem; the command/arg construction is unit-tested (build_venv_create_command / build_pip_install_command) and the idempotency gate is unit-tested (venv_is_provisioned).
async fn ensure_python_env(opts: &GrowwSidecarOptions) -> anyhow::Result<()> {
    if venv_is_provisioned(&opts.venv_dir) {
        return Ok(());
    }
    if let Some(parent) = opts.venv_dir.parent() {
        // Surface (don't swallow) a permissions / disk-full failure here so the
        // operator sees the root cause, instead of a cryptic downstream venv
        // error (security-review MEDIUM 2026-06-19). Non-fatal: venv creation
        // below will still fail-and-retry if the dir is truly unusable.
        if let Err(err) = tokio::fs::create_dir_all(parent).await {
            warn!(
                error = %err,
                dir = %parent.display(),
                "[feeds] groww sidecar: could not create venv parent dir (will retry via provisioning backoff)"
            );
        }
    }
    // Discover the host interpreter (preferring the configured one) so "click
    // Run" self-provisions with no manual Python install on Mac OR AWS.
    let system_python = discover_system_python(&opts.system_python).await;
    info!(
        venv = %opts.venv_dir.display(),
        python = %system_python,
        "[feeds] groww sidecar: provisioning isolated Python env (one-time, automated — discovered interpreter, no manual pip)"
    );

    let (venv_prog, venv_args) = build_venv_create_command(&system_python, &opts.venv_dir);
    let venv_status = tokio::time::timeout(SIDECAR_PROVISION_TIMEOUT, async {
        Command::new(&venv_prog).args(&venv_args).status().await
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "python venv creation timed out after {}s",
            SIDECAR_PROVISION_TIMEOUT.as_secs()
        )
    })??;
    if !venv_status.success() {
        anyhow::bail!(
            "python venv creation (`{system_python} -m venv`) exited with status {venv_status} \
             — on a Debian/Ubuntu host the `python3-venv` package may be missing; \
             Mac (Xcode CLT) and AWS Linux ship it by default"
        );
    }

    let venv_python = venv_python_path(&opts.venv_dir);
    let (pip_prog, pip_args) = build_pip_install_command(&venv_python, &opts.requirements_path);
    let pip_status = tokio::time::timeout(SIDECAR_PROVISION_TIMEOUT, async {
        Command::new(&pip_prog).args(&pip_args).status().await
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "pip install of groww sidecar deps timed out after {}s",
            SIDECAR_PROVISION_TIMEOUT.as_secs()
        )
    })??;
    if !pip_status.success() {
        anyhow::bail!("pip install of groww sidecar deps exited with status {pip_status}");
    }
    info!("[feeds] groww sidecar: Python env provisioned (growwapi + pyotp installed)");
    Ok(())
}

/// Watch a running child until it exits OR the operator disables the Groww
/// feed at runtime. Returns `true` if it was stopped because of a disable
/// toggle (graceful, not a failure), `false` if the child exited on its own
/// (a crash/exit → counts toward restart backoff).
// TEST-EXEMPT: drives a live child process + runtime toggle via tokio::select!; the disable gate it reads (FeedRuntimeState::is_enabled) is unit-tested in the api crate.
async fn supervise_child(
    child: &mut tokio::process::Child,
    feed_runtime: &FeedRuntimeState,
) -> bool {
    loop {
        tokio::select! {
            exit = child.wait() => {
                match exit {
                    Ok(status) => warn!(
                        %status,
                        "[feeds] groww sidecar process exited — will relaunch with backoff"
                    ),
                    Err(err) => warn!(
                        error = %err,
                        "[feeds] groww sidecar wait() failed — will relaunch with backoff"
                    ),
                }
                return false;
            }
            () = sleep(SIDECAR_DISABLE_POLL) => {
                if !feed_runtime.is_enabled(Feed::Groww) {
                    if let Err(err) = child.start_kill() {
                        warn!(error = %err, "[feeds] groww sidecar kill signal failed (already exiting?)");
                    }
                    let _status = child.wait().await;
                    info!("[feeds] groww disabled at runtime — sidecar stopped (will resume on re-enable)");
                    return true;
                }
            }
        }
    }
}

/// Auto-launch + supervise the Groww Python sidecar for the lifetime of the
/// process. Gated on `feeds.groww_enabled` (re-checked every loop, so a runtime
/// toggle pauses/resumes it). Never returns under normal operation.
// TEST-EXEMPT: top-level supervise loop — spawns child processes, fetches SSM credentials, sleeps on backoff; all pure decision primitives (sidecar_restart_backoff / build_*_command / venv_is_provisioned) are unit-tested below and the enable gate is unit-tested in the api crate.
pub async fn run_groww_sidecar_supervisor(
    feed_runtime: Arc<FeedRuntimeState>,
    opts: GrowwSidecarOptions,
) {
    let mut consecutive_failures: u32 = 0;
    loop {
        if !feed_runtime.is_enabled(Feed::Groww) {
            sleep(SIDECAR_DISABLE_POLL).await;
            continue;
        }

        if let Err(err) = ensure_python_env(&opts).await {
            consecutive_failures = consecutive_failures.saturating_add(1);
            let backoff = sidecar_restart_backoff(consecutive_failures);
            error!(
                error = %err,
                backoff_secs = backoff.as_secs(),
                "[feeds] groww sidecar Python env provisioning failed — retrying with backoff"
            );
            sleep(backoff).await;
            continue;
        }

        let creds = match fetch_groww_credentials().await {
            Ok(creds) => creds,
            Err(err) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let backoff = sidecar_restart_backoff(consecutive_failures);
                // Name the RESOLVED env + the EXACT SSM paths read (never the
                // values) so the hint is self-diagnosing — no literal `<env>`.
                let env = resolve_environment().unwrap_or_else(|_| "<unresolved>".to_string());
                let api_key_path = build_ssm_path(
                    &env,
                    tickvault_common::constants::SSM_GROWW_SERVICE,
                    tickvault_common::constants::GROWW_API_KEY_SECRET,
                );
                let totp_path = build_ssm_path(
                    &env,
                    tickvault_common::constants::SSM_GROWW_SERVICE,
                    tickvault_common::constants::GROWW_TOTP_SECRET,
                );
                error!(
                    error = %err,
                    env = %env,
                    api_key_path = %api_key_path,
                    totp_secret_path = %totp_path,
                    backoff_secs = backoff.as_secs(),
                    "[feeds] groww sidecar: SSM credential fetch failed — verify the \
                     api-key + totp-secret exist at the SSM paths above; retrying \
                     with backoff"
                );
                sleep(backoff).await;
                continue;
            }
        };

        let venv_python = venv_python_path(&opts.venv_dir);
        let (prog, args) = build_sidecar_run_command(&venv_python, &opts.script_path);
        let mut cmd = Command::new(&prog);
        cmd.args(&args)
            .env("GROWW_API_KEY", creds.api_key.expose_secret())
            .env("GROWW_TOTP_SECRET", creds.totp_secret.expose_secret())
            .env("GROWW_TICK_FILE", opts.tick_file.to_string_lossy().as_ref())
            // Reap the child deterministically if the supervisor task is dropped
            // (e.g. runtime shutdown / daily AWS stop). Without this, a dropped
            // `Child` leaves the Python sidecar orphaned + still appending to the
            // NDJSON file, which the bridge would re-read with fresh capture_seqs
            // → duplicate Groww rows (hostile-review HIGH 2026-06-19).
            .kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let backoff = sidecar_restart_backoff(consecutive_failures);
                error!(
                    error = %err,
                    program = %prog,
                    backoff_secs = backoff.as_secs(),
                    "[feeds] groww sidecar spawn failed — is python available? retrying with backoff"
                );
                sleep(backoff).await;
                continue;
            }
        };

        info!(
            pid = child.id(),
            "[feeds] groww sidecar launched — appending capture-at-receipt ticks for the bridge"
        );

        let stopped_by_disable = supervise_child(&mut child, &feed_runtime).await;
        if stopped_by_disable {
            // Graceful runtime stop — reset the failure curve so re-enabling
            // relaunches immediately.
            consecutive_failures = 0;
            continue;
        }

        consecutive_failures = consecutive_failures.saturating_add(1);
        let backoff = sidecar_restart_backoff(consecutive_failures);
        info!(
            backoff_secs = backoff.as_secs(),
            consecutive_failures, "[feeds] groww sidecar restart scheduled"
        );
        sleep(backoff).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pick_system_python_prefers_working_preferred() {
        // Preferred works → returned even if other candidates also work.
        let chosen = pick_system_python("python3", |_| true);
        assert_eq!(chosen, "python3");
    }

    #[test]
    fn test_pick_system_python_falls_through_to_first_working_candidate() {
        // Preferred ("python3") absent; only /usr/bin/python3 works → pick it.
        let chosen = pick_system_python("python3", |c| c == "/usr/bin/python3");
        assert_eq!(chosen, "/usr/bin/python3");
    }

    #[test]
    fn test_pick_system_python_respects_candidate_priority_order() {
        // Both a versioned and the system path work → the earlier candidate
        // (python3.13 precedes /usr/bin/python3 in PYTHON_CANDIDATES) wins.
        let chosen =
            pick_system_python("python3", |c| c == "python3.13" || c == "/usr/bin/python3");
        assert_eq!(chosen, "python3.13");
    }

    #[test]
    fn test_pick_system_python_falls_back_to_preferred_when_nothing_works() {
        // Nothing works → return preferred so the caller's clear error path fires.
        let chosen = pick_system_python("python3", |_| false);
        assert_eq!(chosen, "python3");
    }

    #[test]
    fn test_python_candidates_cover_mac_and_aws_locations() {
        // Regression guard: the discovery list must keep the Mac (Xcode CLT +
        // Homebrew) and AWS (PATH) interpreter locations so one-click Run keeps
        // self-provisioning on both.
        for required in [
            "python3",
            "/usr/bin/python3",
            "/opt/homebrew/bin/python3",
            "/usr/local/bin/python3",
        ] {
            assert!(
                PYTHON_CANDIDATES.contains(&required),
                "PYTHON_CANDIDATES must include {required}"
            );
        }
    }

    #[test]
    fn test_sidecar_restart_backoff_zero_failures_is_immediate() {
        assert_eq!(sidecar_restart_backoff(0), Duration::ZERO);
    }

    #[test]
    fn test_sidecar_restart_backoff_is_exponential_then_capped() {
        // 1 -> 2s, 2 -> 4s, 3 -> 8s ...
        assert_eq!(sidecar_restart_backoff(1), Duration::from_secs(2));
        assert_eq!(sidecar_restart_backoff(2), Duration::from_secs(4));
        assert_eq!(sidecar_restart_backoff(3), Duration::from_secs(8));
        assert_eq!(sidecar_restart_backoff(4), Duration::from_secs(16));
        // capped at MAX, never above, even for huge failure counts (no overflow).
        assert_eq!(
            sidecar_restart_backoff(100),
            Duration::from_secs(SIDECAR_RESTART_MAX_SECS)
        );
        assert_eq!(
            sidecar_restart_backoff(u32::MAX),
            Duration::from_secs(SIDECAR_RESTART_MAX_SECS)
        );
    }

    #[test]
    fn test_sidecar_restart_backoff_never_exceeds_max() {
        for failures in 0..1_000u32 {
            assert!(
                sidecar_restart_backoff(failures) <= Duration::from_secs(SIDECAR_RESTART_MAX_SECS)
            );
        }
    }

    #[test]
    fn test_venv_python_path_joins_bin_python3() {
        let p = venv_python_path(Path::new("data/groww/venv"));
        assert_eq!(p, PathBuf::from("data/groww/venv/bin/python3"));
    }

    #[test]
    fn test_venv_is_provisioned_false_for_missing_dir() {
        // A path that cannot exist on the test host.
        assert!(!venv_is_provisioned(Path::new(
            "/nonexistent-tickvault-groww-venv-xyz"
        )));
    }

    #[test]
    fn test_venv_is_provisioned_true_when_python_present() {
        let dir = std::env::temp_dir().join(format!("tv-groww-venv-test-{}", std::process::id()));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("create test bin dir");
        let py = bin.join("python3");
        std::fs::write(&py, b"#!/bin/sh\n").expect("write fake python");
        assert!(venv_is_provisioned(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_build_venv_create_command_parts() {
        let (prog, args) = build_venv_create_command("python3", Path::new("data/groww/venv"));
        assert_eq!(prog, "python3");
        assert_eq!(args, vec!["-m", "venv", "data/groww/venv"]);
    }

    #[test]
    fn test_build_pip_install_command_uses_venv_python_and_requirements() {
        let (prog, args) = build_pip_install_command(
            Path::new("data/groww/venv/bin/python3"),
            Path::new("scripts/groww-sidecar/requirements.txt"),
        );
        assert_eq!(prog, "data/groww/venv/bin/python3");
        assert_eq!(args[0], "-m");
        assert_eq!(args[1], "pip");
        assert_eq!(args[2], "install");
        assert!(args.contains(&"-r".to_string()));
        assert!(
            args.last().unwrap().ends_with("requirements.txt"),
            "requirements path must be the final arg"
        );
    }

    #[test]
    fn test_build_sidecar_run_command_runs_script_with_venv_python() {
        let (prog, args) = build_sidecar_run_command(
            Path::new("data/groww/venv/bin/python3"),
            Path::new("scripts/groww-sidecar/groww_sidecar.py"),
        );
        assert_eq!(prog, "data/groww/venv/bin/python3");
        assert_eq!(args, vec!["scripts/groww-sidecar/groww_sidecar.py"]);
    }

    #[test]
    fn test_groww_sidecar_options_default_paths() {
        let opts = GrowwSidecarOptions::default();
        assert_eq!(opts.system_python, "python3");
        assert_eq!(opts.venv_dir, PathBuf::from("data/groww/venv"));
        assert_eq!(
            opts.script_path,
            PathBuf::from("scripts/groww-sidecar/groww_sidecar.py")
        );
        assert_eq!(
            opts.tick_file,
            PathBuf::from(crate::groww_bridge::GROWW_TICK_FILE_DEFAULT)
        );
    }
}
