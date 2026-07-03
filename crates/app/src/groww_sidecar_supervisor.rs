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
//!    `data/groww/venv` and `pip install`s `growwapi` + `boto3` into it ONCE
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
//! 2. **hands the child the SSM PARAMETER PATH of the shared access token**
//!    (`GROWW_SSM_TOKEN_PARAM=/tickvault/<env>/groww/access-token`) — shared
//!    token-minter lock 2026-07-02
//!    (`.claude/rules/project/groww-shared-token-minter-2026-07-02.md`): the
//!    bruteX `groww-token-minter` Lambda is the SOLE minter; the sidecar
//!    reads that ONE parameter fresh on every connect cycle (read-only reader
//!    role) and NEVER mints. The supervisor no longer touches any Groww
//!    credential (the api-key/totp-secret params are Lambda-only by IAM);
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
//! - NO secret crosses the process boundary (2026-07-02 token-minter sync):
//!   the child env carries only the SSM parameter PATH of the access token —
//!   a non-secret string — and the sidecar reads the SecureString itself via
//!   the read-only reader role. The former env-injected credentials (and the
//!   `/proc/<pid>/environ` residual MEDIUM they carried) are gone.
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
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{error, info, warn};

use tickvault_api::feed_state::{Feed, FeedRuntimeState};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed_health::FeedHealthRegistry;
use tickvault_core::auth::secret_manager::{build_ssm_path, resolve_environment};
use tickvault_core::notification::{NotificationEvent, NotificationService};

/// Classification of one diagnostic line the Python sidecar prints, used by the
/// supervisor to route the line to `tracing` at the right level and to decide
/// whether it is an operator-actionable feed reject (→ feed_health Down + ONE
/// Telegram event) or just informational. Pure, O(1) (case-insensitive
/// substring match against the REAL strings the sidecar emits — see
/// `scripts/groww-sidecar/groww_sidecar.py`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidecarLineClass {
    /// The Groww account has no LIVE market-data feed entitlement (the sidecar's
    /// SILENT-FEED watchdog), or the line names a permissions/authorization
    /// problem. The socket connects but streams nothing.
    EntitlementRejected,
    /// An auth-phase reject (`groww sidecar error [auth]`) — the shared SSM
    /// access token is stale/unusable (the sidecar NEVER mints; it re-reads
    /// the bruteX-minted token from SSM each cycle, so a persistent auth
    /// reject means the daily minter Lambda has not produced a fresh token).
    AuthRejected,
    /// A generic sidecar error (feed-connect / subscribe / consume phase, or the
    /// SDK's bare `Error:` NATS line).
    Error,
    /// A subscribe-confirmation line (`subscribed N stocks + M indices`).
    Subscribed,
    /// A positive/streaming line (`groww auth OK`, NDJSON append).
    Streaming,
    /// Anything else — informational.
    Info,
}

impl SidecarLineClass {
    /// True for the classes that are operator-actionable feed rejects — the
    /// supervisor marks Groww rejected in feed_health + fires ONE Telegram event
    /// for these (edge-triggered per running child). `Subscribed`/`Streaming`/
    /// `Info` are tracing-only.
    #[must_use]
    pub const fn triggers_alert(self) -> bool {
        matches!(
            self,
            Self::EntitlementRejected | Self::AuthRejected | Self::Error
        )
    }

    /// True ONLY for the classes that represent a CONFIRMED auth / entitlement
    /// reject — the cases where the operator must "refresh the Groww SSM
    /// api-key". This is DELIBERATELY narrower than [`Self::triggers_alert`].
    ///
    /// The generic [`Self::Error`] class (a NATS/SDK reconnect line, a recovered
    /// `traceback`/`error:`, the bare SDK `Error:` line) is a TRANSIENT /
    /// degraded condition, NOT a credential fault — so it must NOT latch the
    /// `auth_rejected` health bit. Before this split, `Error` shared the same bit
    /// as a real reject and produced a false "refresh the api-key" RED for a
    /// non-auth condition (the root of the false-auth family). `Error` still
    /// fires its visibility alert (via `triggers_alert`) + its `error!` log; it
    /// just no longer claims the credential is bad.
    #[must_use]
    pub const fn sets_auth_rejected(self) -> bool {
        matches!(self, Self::EntitlementRejected | Self::AuthRejected)
    }

    /// True for the positive / streaming classes that confirm the feed is alive
    /// again — a NON-TICK recovery edge. The supervisor uses this to clear a
    /// stale `auth_rejected` flag the instant the sidecar reports `groww auth OK`
    /// / an NDJSON append, so a tick-silent window can no longer latch the
    /// false-RED for the whole session. `Subscribed` is a one-shot startup line,
    /// not proof the stream recovered, so only `Streaming` qualifies.
    #[must_use]
    pub const fn clears_auth_rejected(self) -> bool {
        matches!(self, Self::Streaming)
    }

    /// The fixed, plain-English reason shown to the operator for an
    /// alert-triggering class. A FIXED `&'static str` per class — never the raw
    /// child line — so no runtime/credential text can reach Telegram
    /// (defense-in-depth). `None` for the non-alert classes.
    #[must_use]
    pub const fn alert_reason(self) -> Option<&'static str> {
        match self {
            Self::EntitlementRejected => Some(
                "account lacks a live market-data feed entitlement (or feed permissions denied)",
            ),
            Self::AuthRejected => Some(
                "authentication rejected — the shared access token is stale/unusable; \
                 check the bruteX groww-token-minter Lambda's last daily mint",
            ),
            Self::Error => Some("the feed reported an error and is retrying"),
            Self::Subscribed | Self::Streaming | Self::Info => None,
        }
    }
}

/// True for the sidecar's SILENT-FEED WATCHDOG lines (`SILENT FEED …` /
/// `STILL SILENT …`) — the time-driven "subscribed but received no records in
/// 30s" diagnostic. These are EXPECTED + benign when the market is closed
/// (silence is normal after-hours, exactly what the `/feeds` page now says — PR
/// #1260) but ARE a real problem during market hours (entitlement gap / reject).
///
/// This is DELIBERATELY narrower than [`SidecarLineClass::EntitlementRejected`]:
/// a line that NAMES a hard entitlement / permission / authorization problem
/// (e.g. `account lacks a live market-data feed entitlement`, `NATS permissions
/// violation`) is a real config fault at ANY time and is NOT a silent-feed
/// watchdog line — so it keeps its `error!` level always. Pure, O(1),
/// case-insensitive substring match against the REAL watchdog strings the
/// sidecar prints (see `scripts/groww-sidecar/groww_sidecar.py`).
#[must_use]
pub fn is_silent_feed_diagnostic(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("silent feed") || l.contains("still silent")
}

/// Tracing [`Level`](tracing::Level) for a SILENT-FEED watchdog diagnostic,
/// decided purely by whether the market is open. During market hours prolonged
/// silence IS abnormal → `ERROR` (operator must be alerted). Outside market
/// hours silence is EXPECTED → `INFO` (a calm "idle — market closed" note that
/// must NOT land in `errors.jsonl` / `errors.log` as an ERROR). Pure, O(1) —
/// the market-hours decision is injected so this is unit-testable without a
/// live clock or sidecar.
#[must_use]
pub fn silent_feed_diagnostic_level(market_open: bool) -> tracing::Level {
    if market_open {
        tracing::Level::ERROR
    } else {
        tracing::Level::INFO
    }
}

/// Classify one sidecar diagnostic line. Pure, O(1), case-insensitive substring
/// match against the REAL strings `scripts/groww-sidecar/groww_sidecar.py`
/// prints. Order matters: the most-specific / most-actionable class wins.
#[must_use]
pub fn classify_sidecar_line(line: &str) -> SidecarLineClass {
    let l = line.to_ascii_lowercase();
    // Auth reject is the most specific cause — check before the generic error.
    // "access token stale" is the sidecar's 10-min minter-dead marker (2026-07-02
    // adversarial finding H1: without this arm the marker classified Info and the
    // promised feed-health + Telegram routing never fired).
    if l.contains("error [auth]") || l.contains("access token stale") {
        return SidecarLineClass::AuthRejected;
    }
    // Entitlement / permissions: the SILENT-FEED watchdog + permissions text.
    if l.contains("silent feed")
        || l.contains("still silent")
        || l.contains("entitlement")
        || l.contains("permission")
        || l.contains("authoriz")
    {
        return SidecarLineClass::EntitlementRejected;
    }
    // Any other error phase, or the SDK's bare "Error:" NATS line.
    if l.contains("sidecar error")
        || l.contains("traceback")
        || l.starts_with("error:")
        || l.contains(" error:")
    {
        return SidecarLineClass::Error;
    }
    if l.contains("subscribed ") {
        return SidecarLineClass::Subscribed;
    }
    if l.contains("auth ok") || l.contains("appending ndjson") || l.contains("→ appending") {
        return SidecarLineClass::Streaming;
    }
    SidecarLineClass::Info
}

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

/// Feed-level stall threshold (seconds). A live feed sidecar that is ALIVE but
/// has streamed NO tick across its WHOLE subscribed universe for longer than this
/// — during market hours — is treated as a dead socket and restarted (the
/// silently-closed NATS socket that left Groww dead at 10:31 IST). Set to the same
/// order as [`tickvault_common::feed_health::FEED_STALE_TICK_SECS`] (30s): at
/// market open ticks flow every second across the ~767-SID universe, so a 30s
/// feed-level gap is a real dead socket, not illiquidity, yet a brief lull never
/// false-restarts. The arm ALSO requires a KNOWN last-tick, so a cold pre-open
/// feed (no first tick yet) is never killed by this watchdog.
pub const FEED_STALL_RESTART_SECS: u64 = 30;

/// Stall-watchdog poll cadence (seconds). Cheap: one relaxed atomic load per tick.
pub const STALL_WATCHDOG_POLL_SECS: u64 = 1;

/// Stall-watchdog poll cadence.
pub const STALL_WATCHDOG_POLL: Duration = Duration::from_secs(STALL_WATCHDOG_POLL_SECS);

/// Restart-STORM window (seconds). Rapid stall-restarts inside this sliding window
/// are counted; exceeding [`STALL_RESTART_STORM_MAX`] escalates + applies a backoff
/// ceiling (never a permanent give-up during market hours).
pub const STALL_RESTART_STORM_WINDOW_SECS: u64 = 300;

/// Max stall-restarts allowed inside [`STALL_RESTART_STORM_WINDOW_SECS`] before the
/// watchdog escalates (`error!(code=FEED-STALL-01)` + the rapid-restart count) and
/// stops resetting the failure curve — so a flapping socket (reconnect→re-drop) is
/// bounded to the existing 60s [`SIDECAR_RESTART_MAX_SECS`] ceiling instead of a
/// tight kill loop. It NEVER permanently gives up during market hours.
pub const STALL_RESTART_STORM_MAX: u32 = 5;

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
    /// The connect+subscribe PROOF status file the sidecar writes atomically and
    /// the bridge reads (operator 2026-06-28). Passed to the child as
    /// `GROWW_STATUS_FILE`. Carries ONLY counts + an event tag + a timestamp —
    /// never a credential.
    pub status_file: PathBuf,
    /// §34 auto-scale (PR-2): the connection id this child owns. `None` =
    /// the single-conn path (byte-identical to pre-scale behavior — no new
    /// env vars, no argv, same pkill needle). `Some(n)` = fleet child `n`:
    /// per-conn env (`GROWW_CONN_ID` / `GROWW_SHARD_SPEC` / `GROWW_WATCH_DIR`),
    /// per-conn argv (`--conn-id n` — the pkill needle distinguisher so
    /// relaunching conn n never reaps its siblings), per-conn paths.
    pub conn_id: Option<usize>,
    /// §34: the per-conn watch DIRECTORY (`GROWW_WATCH_DIR` env). `None` =
    /// the sidecar's built-in default (`data/groww`).
    pub watch_dir: Option<PathBuf>,
    /// §34: human-readable shard identity for the child's log lines
    /// (`GROWW_SHARD_SPEC` env). Counts + ranges only — never a credential.
    pub shard_spec: Option<String>,
}

impl Default for GrowwSidecarOptions {
    fn default() -> Self {
        Self {
            system_python: SYSTEM_PYTHON_DEFAULT.to_string(),
            venv_dir: PathBuf::from(GROWW_SIDECAR_VENV_DEFAULT),
            script_path: PathBuf::from(GROWW_SIDECAR_SCRIPT_DEFAULT),
            requirements_path: PathBuf::from(GROWW_SIDECAR_REQUIREMENTS_DEFAULT),
            tick_file: PathBuf::from(crate::groww_bridge::GROWW_TICK_FILE_DEFAULT),
            status_file: PathBuf::from(crate::groww_bridge::GROWW_STATUS_FILE_DEFAULT),
            conn_id: None,
            watch_dir: None,
            shard_spec: None,
        }
    }
}

/// §34 auto-scale (PR-2 Item 5): derives one fleet child's options from the
/// base options + the shard's file layout. Pure — path/env math only. The
/// venv, script, and requirements are SHARED (one provision, N children);
/// tick/status/watch paths are per-conn (disjoint capture chains).
#[must_use]
pub fn shard_sidecar_options(
    base: &GrowwSidecarOptions,
    files: &crate::groww_bridge::GrowwShardFiles,
    shard_spec: String,
) -> GrowwSidecarOptions {
    GrowwSidecarOptions {
        system_python: base.system_python.clone(),
        venv_dir: base.venv_dir.clone(),
        script_path: base.script_path.clone(),
        requirements_path: base.requirements_path.clone(),
        tick_file: files.tick_file.clone(),
        status_file: files.status_file.clone(),
        conn_id: Some(files.conn_id),
        watch_dir: Some(files.watch_dir.clone()),
        shard_spec: Some(shard_spec),
    }
}

/// §34: the pkill pattern that reaps ONLY this child's orphaned prior
/// process. Single-conn = the script path (pre-scale behavior, unchanged).
/// Fleet child n = `<script> --conn-id n$` — the `$` anchor plus the argv
/// suffix means relaunching conn 3 can never match conn 30 or a sibling.
#[must_use]
pub fn orphan_reap_needle(script_path: &Path, conn_id: Option<usize>) -> String {
    let script = script_path.to_string_lossy();
    match conn_id {
        None => script.into_owned(),
        Some(n) => format!("{script} --conn-id {n}$"),
    }
}

/// §34: the child argv for a given conn. Single-conn = script only
/// (byte-identical to pre-scale). Fleet child = `script --conn-id n` so the
/// cmdline is per-conn distinguishable (the reap needle above).
#[must_use]
pub fn sidecar_argv(script_path: &Path, conn_id: Option<usize>) -> Vec<String> {
    let mut args = vec![script_path.to_string_lossy().into_owned()];
    if let Some(n) = conn_id {
        args.push("--conn-id".to_string());
        args.push(n.to_string());
    }
    args
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

/// FEED-AGNOSTIC stall decision (FEED-STALL-01). Returns `true` ONLY when the
/// feed is enabled, the market is open, a last-tick is KNOWN, and the feed-level
/// last-tick age exceeds `threshold_secs`. Pure + O(1); takes the inputs for ANY
/// feed — there is NO feed-specific branch, so a future feed #3…N inherits the
/// identical stall→restart logic with ZERO new code.
///
/// `last_tick_age_secs == None` (no first tick yet — a cold pre-open feed) returns
/// `false`: the watchdog must NEVER kill a feed that has not streamed its first
/// tick (that case is the silent-feed diagnostic's job, not a kill loop). The
/// `market_open` gate is start-inclusive / end-exclusive (the caller passes the
/// existing `is_within_market_hours_ist()`), so a stall AT 15:30:00 close does NOT
/// restart. The threshold is `>` (strict), so exactly-at-threshold does NOT fire.
#[must_use]
pub fn should_restart_on_stall(
    last_tick_age_secs: Option<u64>,
    market_open: bool,
    enabled: bool,
    threshold_secs: u64,
) -> bool {
    if !enabled || !market_open {
        return false;
    }
    match last_tick_age_secs {
        None => false,
        Some(age) => age > threshold_secs,
    }
}

/// Outcome of a supervised feed-supervisor task, used by [`should_respawn_supervisor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupervisorJoinOutcome {
    /// The supervisor task panicked (a `JoinError` with `is_panic()`).
    Panicked,
    /// The supervisor's `run_*` future returned (it should loop forever, so a
    /// return is unexpected).
    Returned,
    /// The task was cancelled / aborted (runtime shutdown) — a clean teardown.
    Cancelled,
}

/// FEED-AGNOSTIC supervisor-respawn decision (FEED-SUPERVISOR-01). The feed
/// supervisor loop should run forever; if its task panics or unexpectedly returns
/// it MUST be respawned so the stall-watchdog can never die silently (WS-GAP-05 /
/// DISK-WATCHER-01 pattern). A genuine cancel (runtime shutdown) is NOT respawned.
/// Pure + O(1), feed-agnostic.
#[must_use]
pub const fn should_respawn_supervisor(outcome: SupervisorJoinOutcome) -> bool {
    matches!(
        outcome,
        SupervisorJoinOutcome::Panicked | SupervisorJoinOutcome::Returned
    )
}

/// Tracks rapid stall-restarts in a sliding window so a flapping socket
/// (reconnect→re-drop repeatedly) is bounded instead of a tight kill loop. Pure
/// state machine, O(1) per event, no allocation — the caller drives it with a
/// monotonic-ish `now_secs` (the supervisor uses wall-clock IST seconds, which is
/// fine: a backward clock step only widens the window, never shrinks it below 0).
#[derive(Debug, Clone, Copy, Default)]
pub struct StallRestartStorm {
    window_start_secs: u64,
    count_in_window: u32,
}

impl StallRestartStorm {
    /// Record one stall-restart at `now_secs`. Returns `true` if this restart
    /// pushed the count OVER [`STALL_RESTART_STORM_MAX`] inside
    /// [`STALL_RESTART_STORM_WINDOW_SECS`] (the escalation edge). The window resets
    /// once `now_secs` advances past it. Pure + O(1).
    pub fn record_and_is_storm(&mut self, now_secs: u64) -> bool {
        if now_secs.saturating_sub(self.window_start_secs) > STALL_RESTART_STORM_WINDOW_SECS {
            // Window elapsed → start a fresh window with this restart.
            self.window_start_secs = now_secs;
            self.count_in_window = 1;
            return false;
        }
        self.count_in_window = self.count_in_window.saturating_add(1);
        self.count_in_window > STALL_RESTART_STORM_MAX
    }

    /// The current count of restarts inside the active window (for the escalation
    /// log). Pure.
    #[must_use]
    pub const fn count_in_window(&self) -> u32 {
        self.count_in_window
    }
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

/// Path of the requirements-fingerprint marker written into the venv after a
/// successful `pip install`. When `requirements.txt` changes (e.g. the
/// 2026-07-02 pyotp→boto3 swap for the shared token-minter sync), the stored
/// fingerprint no longer matches and provisioning re-runs `pip install` into
/// the EXISTING venv — otherwise an already-deployed box would keep a stale
/// dependency set forever and the sidecar would die on import.
#[must_use]
pub fn requirements_marker_path(venv_dir: &Path) -> PathBuf {
    venv_dir.join(".requirements-fingerprint")
}

/// FNV-1a 64-bit fingerprint of the requirements manifest contents. Pure —
/// same non-cryptographic content-fingerprint family the codebase already
/// uses for log-signature hashing (collision here only means a skipped
/// re-install, corrected by deleting the marker; not security-relevant).
#[must_use]
pub fn requirements_fingerprint(contents: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in contents.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

/// Pure decision: does the venv need (re-)provisioning? True when the python
/// interpreter is missing (fresh box) OR the stored requirements fingerprint
/// differs from the current manifest's (deps changed since the last install).
#[must_use]
pub fn venv_needs_provisioning(
    python_present: bool,
    stored_fingerprint: Option<&str>,
    current_fingerprint: &str,
) -> bool {
    if !python_present {
        return true;
    }
    stored_fingerprint != Some(current_fingerprint)
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

/// Provision the isolated venv + dependencies if not already present OR if the
/// requirements manifest changed since the last install (fingerprint marker —
/// so a dependency swap like the 2026-07-02 pyotp→boto3 token-minter sync
/// reaches already-deployed boxes automatically).
/// Idempotent: returns early when the venv python exists AND the stored
/// fingerprint matches the current `requirements.txt`.
// TEST-EXEMPT: spawns `python -m venv` + `pip install` child processes + filesystem; the command/arg construction is unit-tested (build_venv_create_command / build_pip_install_command) and the idempotency gates are unit-tested (venv_is_provisioned / venv_needs_provisioning / requirements_fingerprint).
async fn ensure_python_env(opts: &GrowwSidecarOptions) -> anyhow::Result<()> {
    let python_present = venv_is_provisioned(&opts.venv_dir);
    let requirements_contents = tokio::fs::read_to_string(&opts.requirements_path)
        .await
        .unwrap_or_default();
    let current_fingerprint = requirements_fingerprint(&requirements_contents);
    let marker = requirements_marker_path(&opts.venv_dir);
    let stored = tokio::fs::read_to_string(&marker).await.ok();
    if !venv_needs_provisioning(python_present, stored.as_deref(), &current_fingerprint) {
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

    // A dependency change re-provisions into the EXISTING venv (pip install is
    // idempotent) — only a missing interpreter needs the venv created.
    if !python_present {
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
    // Record what was installed so a future requirements change re-triggers
    // this path. Best-effort: a write failure only means a redundant future
    // pip install (idempotent), never a broken env.
    if let Err(err) = tokio::fs::write(&marker, &current_fingerprint).await {
        warn!(
            error = %err,
            marker = %marker.display(),
            "[feeds] groww sidecar: could not write requirements fingerprint marker"
        );
    }
    info!("[feeds] groww sidecar: Python env provisioned (growwapi + boto3 installed)");
    Ok(())
}

/// Drain one captured child pipe line-by-line. Each line is classified and
/// forwarded to `tracing` at the right level; on the FIRST alert-class line
/// (edge-triggered via the shared `alerted` latch) it marks Groww rejected in
/// `feed_health` (→ the `/feeds` dashboard shows the actionable Down) and fires
/// ONE typed Telegram event with a FIXED per-class reason (never the raw child
/// text). The task ends naturally when the pipe closes (child exit). Non-blocking,
/// cold-path. `pipe_name` is only a tracing label.
fn spawn_pipe_drain<R>(
    pipe: R,
    pipe_name: &'static str,
    feed_health: Arc<FeedHealthRegistry>,
    notifier: Option<Arc<NotificationService>>,
    alerted: Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(pipe).lines();
        // next_line() is cancel-safe and ends with Ok(None) on EOF (pipe closed).
        while let Ok(Some(line)) = lines.next_line().await {
            let class = classify_sidecar_line(&line);
            // The sidecar's SILENT-FEED WATCHDOG line ("subscribed N but received
            // NO records in 30s") is benign when the market is closed (silence is
            // expected after-hours — matches the `/feeds` page, PR #1260) but is a
            // real problem during market hours. Decide its level + whether it
            // alerts by the SAME market-hours helper #1260 used, so the LOG channel
            // agrees with the page instead of flooding errors.jsonl with red lines
            // every 60s after close. All OTHER lines (genuine entitlement /
            // permission / auth / traceback / generic error) keep their class-implied
            // level + alerting at ALL times.
            let silent_feed_diag = is_silent_feed_diagnostic(&line);
            let market_open = tickvault_common::market_hours::is_within_market_hours_ist();
            // Forward the child's OWN (already-redacted) line to tracing at the
            // level its class implies, so it reaches the 5-sink → CloudWatch.
            if silent_feed_diag {
                // INFO when the market is closed (idle is normal), ERROR during
                // market hours (prolonged silence is abnormal).
                match silent_feed_diagnostic_level(market_open) {
                    tracing::Level::ERROR => {
                        error!(stream = pipe_name, "[feeds] groww sidecar: {line}");
                    }
                    _ => {
                        info!(
                            stream = pipe_name,
                            market_open,
                            "[feeds] groww idle — market closed, awaiting open: {line}"
                        );
                    }
                }
            } else {
                match class {
                    SidecarLineClass::AuthRejected
                    | SidecarLineClass::EntitlementRejected
                    | SidecarLineClass::Error => {
                        error!(stream = pipe_name, "[feeds] groww sidecar: {line}");
                    }
                    SidecarLineClass::Subscribed | SidecarLineClass::Streaming => {
                        info!(stream = pipe_name, "[feeds] groww sidecar: {line}");
                    }
                    SidecarLineClass::Info => {
                        info!(stream = pipe_name, "[feeds] groww sidecar: {line}");
                    }
                }
            }
            // A confirmed streaming / auth-OK line is a NON-TICK recovery edge:
            // clear any stale auth_rejected so a tick-silent window can't latch
            // the false-RED for the whole session (edge-triggered, no-op when
            // already clear). Reset the per-child `alerted` latch too so a later
            // genuine reject can re-fire its one-shot alert.
            if class.clears_auth_rejected() {
                feed_health.clear_auth_rejected(Feed::Groww);
                alerted.store(false, std::sync::atomic::Ordering::Relaxed);
            }
            // A SILENT-FEED watchdog line only counts as an operator-actionable
            // reject DURING market hours — after-hours silence must not mark the
            // feed Down or fire a Telegram page (consistent with #1260's page).
            // Every other alert class fires its side-effects at all times.
            let triggers_alert = class.triggers_alert() && (!silent_feed_diag || market_open);
            if triggers_alert {
                // Edge-trigger: fire the operator-facing side-effects ONCE per
                // running child, even if the same error prints 100×.
                if !alerted.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    // Latch the actionable auth-rejected RED ONLY for a CONFIRMED
                    // auth / entitlement reject — NOT for the generic transient
                    // `Error` class. `Error` keeps its `error!` log + this Telegram
                    // visibility alert, but must not claim the credential is bad
                    // (the root false-"refresh the api-key" fix).
                    if class.sets_auth_rejected() {
                        feed_health.set_auth_rejected(Feed::Groww, true);
                    }
                    if let (Some(notifier), Some(reason)) = (&notifier, class.alert_reason()) {
                        notifier.notify(NotificationEvent::GrowwSidecarRejected {
                            reason: reason.to_string(),
                        });
                    }
                }
            }
        }
    })
}

/// Why [`supervise_child`] returned, so the caller can tune the backoff:
/// graceful disable + stall-restart respawn immediately (reset the failure
/// curve), a crash/exit backs off normally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SuperviseOutcome {
    /// Stopped because the operator disabled the feed at runtime (graceful).
    DisabledByToggle,
    /// FEED-STALL-01: the feed-agnostic stall-watchdog killed the alive-but-silent
    /// child so the existing relaunch loop respawns it (fast — unless a STORM).
    StallRestart,
    /// The child exited on its own (a crash/exit → normal restart backoff).
    Exited,
}

/// Watch a running child until it exits, the operator disables the feed at
/// runtime, OR the FEED-AGNOSTIC stall-watchdog detects an alive-but-silent feed
/// (FEED-STALL-01), while draining its stdout + stderr (each line → tracing,
/// classified; alert classes → feed_health Down + ONE Telegram event). The two
/// drain tasks are aborted on return so they never leak across restarts.
///
/// `feed` makes this COMMON: the stall-watchdog reads `feed`'s slot of the
/// `Feed`-keyed `feed_health` registry, so a future feed inherits the identical
/// stall→restart with no new code. `now_ist_nanos` is injected (a fn ptr) so the
/// pure decision is testable; production passes a wall-clock IST-nanos closure.
// TEST-EXEMPT: drives a live child process + runtime toggle + stall poll via tokio::select!; the disable gate it reads (FeedRuntimeState::is_enabled) is unit-tested in the api crate; the pure decision fns (should_restart_on_stall, StallRestartStorm, classify_sidecar_line) are unit-tested below.
async fn supervise_child(
    child: &mut tokio::process::Child,
    feed: Feed,
    feed_runtime: &FeedRuntimeState,
    feed_health: &Arc<FeedHealthRegistry>,
    notifier: &Option<Arc<NotificationService>>,
    now_ist_nanos: fn() -> i64,
    storm: &mut StallRestartStorm,
) -> SuperviseOutcome {
    // One latch per running child so the alert fires at most once per instance.
    let alerted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut drains: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        drains.push(spawn_pipe_drain(
            stdout,
            "stdout",
            Arc::clone(feed_health),
            notifier.clone(),
            Arc::clone(&alerted),
        ));
    }
    if let Some(stderr) = child.stderr.take() {
        drains.push(spawn_pipe_drain(
            stderr,
            "stderr",
            Arc::clone(feed_health),
            notifier.clone(),
            Arc::clone(&alerted),
        ));
    }
    let abort_drains = || {
        for h in &drains {
            h.abort();
        }
    };
    loop {
        tokio::select! {
            exit = child.wait() => {
                match exit {
                    Ok(status) => warn!(
                        %status,
                        feed = feed.as_str(),
                        "[feeds] sidecar process exited — will relaunch with backoff"
                    ),
                    Err(err) => warn!(
                        error = %err,
                        feed = feed.as_str(),
                        "[feeds] sidecar wait() failed — will relaunch with backoff"
                    ),
                }
                abort_drains();
                return SuperviseOutcome::Exited;
            }
            () = sleep(SIDECAR_DISABLE_POLL) => {
                // Disable toggle WINS over a stall (operator intent first): a
                // runtime disable gracefully stops the child and resets the curve.
                if !feed_runtime.is_enabled(feed) {
                    if let Err(err) = child.start_kill() {
                        warn!(error = %err, feed = feed.as_str(), "[feeds] sidecar kill signal failed (already exiting?)");
                    }
                    let _status = child.wait().await;
                    abort_drains();
                    info!(feed = feed.as_str(), "[feeds] disabled at runtime — sidecar stopped (will resume on re-enable)");
                    return SuperviseOutcome::DisabledByToggle;
                }
            }
            // FEED-STALL-01: the FEED-AGNOSTIC stall-watchdog. Poll the feed-level
            // last-tick age across the WHOLE subscribed universe; an alive-but-
            // silent feed during market hours is a dead socket (not illiquidity —
            // ticks flow every second across ~767 SIDs at open), so kill + relaunch.
            () = sleep(STALL_WATCHDOG_POLL) => {
                // Re-read enabled INSIDE the arm so a disable toggle in the same
                // instant is never overridden by the kill (watchdog doesn't fight
                // the toggle).
                let enabled = feed_runtime.is_enabled(feed);
                let market_open = tickvault_common::market_hours::is_within_market_hours_ist();
                let age = feed_health.last_tick_age_secs(feed, now_ist_nanos());
                if should_restart_on_stall(age, market_open, enabled, FEED_STALL_RESTART_SECS) {
                    let now_secs = (now_ist_nanos() / 1_000_000_000).max(0) as u64;
                    let is_storm = storm.record_and_is_storm(now_secs);
                    let stall_secs = age.unwrap_or(0);
                    if is_storm {
                        // Flapping socket (reconnect→re-drop): escalate + let the
                        // existing 60s backoff ceiling apply (the caller does NOT
                        // reset the failure curve on a storm) — but NEVER give up.
                        error!(
                            code = ErrorCode::FeedStall01SidecarRestarted.code_str(),
                            feed = feed.as_str(),
                            stall_secs,
                            rapid_restarts = storm.count_in_window(),
                            "[feeds] sidecar STALL-RESTART STORM — alive but silent and flapping; \
                             applying backoff ceiling (still retrying, never giving up in market hours)"
                        );
                    } else {
                        warn!(
                            code = ErrorCode::FeedStall01SidecarRestarted.code_str(),
                            feed = feed.as_str(),
                            stall_secs,
                            "[feeds] sidecar ALIVE but silent across the universe during market hours — \
                             killing + relaunching (re-auth + re-subscribe)"
                        );
                    }
                    metrics::counter!(
                        "tv_feed_sidecar_stall_restart_total",
                        "feed" => feed.as_str(),
                    )
                    .increment(1);
                    if let Err(err) = child.start_kill() {
                        warn!(error = %err, feed = feed.as_str(), "[feeds] sidecar stall-kill signal failed (already exiting?)");
                    }
                    let _status = child.wait().await;
                    abort_drains();
                    return if is_storm {
                        SuperviseOutcome::Exited
                    } else {
                        SuperviseOutcome::StallRestart
                    };
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
    // Live-feed health (operator 2026-06-22): on a sidecar auth/entitlement/error
    // diagnostic, mark Groww rejected so `GET /api/feeds/health` + the `/feeds`
    // page show the actionable Down with the cause — never a silent 0-ticks.
    feed_health: Arc<FeedHealthRegistry>,
    // Telegram dispatcher, DEFERRED: the supervisor is spawned early at boot
    // (before the notifier is built), so it receives a shared slot that main
    // fills once `NotificationService` is ready. The supervisor resolves it
    // lazily at each child launch; an empty slot (defensive / test / pre-notifier
    // boot window) is a no-op — feed_health + tracing still fire. Fires ONE
    // `GrowwSidecarRejected` event per running child reject so the operator sees
    // WHY Groww has 0 ticks.
    notifier: Arc<arc_swap::ArcSwapOption<NotificationService>>,
) {
    let mut consecutive_failures: u32 = 0;
    // Sliding-window tracker that bounds a flapping (reconnect→re-drop) socket so
    // rapid stall-restarts escalate + hit the backoff ceiling instead of a tight
    // kill loop. Persists across child launches for the supervisor's lifetime.
    let mut storm = StallRestartStorm::default();
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

        // Shared token-minter architecture (operator lock 2026-07-02 —
        // `.claude/rules/project/groww-shared-token-minter-2026-07-02.md`):
        // the supervisor NEVER fetches Groww credentials and NEVER mints. It
        // hands the sidecar the SSM PARAMETER PATH of the access token minted
        // daily by the bruteX `groww-token-minter` Lambda; the sidecar reads
        // that ONE parameter (read-only, reader-role via the default AWS
        // credential chain) FRESH on every connect cycle — which is what makes
        // the 06:00 IST daily token reset + re-read-on-401 self-healing.
        let env = resolve_environment().unwrap_or_else(|_| "<unresolved>".to_string());
        let token_param_path = build_ssm_path(
            &env,
            tickvault_common::constants::SSM_GROWW_SERVICE,
            tickvault_common::constants::GROWW_ACCESS_TOKEN_SECRET,
        );

        // Orphan reap (2026-07-02 adversarial finding M5): a SIGKILLed tickvault
        // never runs kill_on_drop, leaving an ORPHANED sidecar still appending to
        // the NDJSON (duplicate capture_seq rows) — and a pre-deploy orphan could
        // even carry the retired mint code. Best-effort pkill of any process
        // running our sidecar script before spawning the fresh one (single-app
        // host; matches the exact script path, nothing else).
        // §34: per-conn needle when this is a fleet child — relaunching conn n
        // must never reap a healthy sibling (see `orphan_reap_needle`).
        let script_needle = orphan_reap_needle(&opts.script_path, opts.conn_id);
        if let Ok(status) = Command::new("pkill")
            .args(["-f", &script_needle])
            .status()
            .await
            && status.success()
        {
            warn!(
                script = %script_needle,
                "[feeds] groww sidecar: reaped an orphaned prior sidecar process \
                 before launch (stale process from a previous run)"
            );
        }

        let venv_python = venv_python_path(&opts.venv_dir);
        let (prog, _single_args) = build_sidecar_run_command(&venv_python, &opts.script_path);
        // §34: fleet children carry `--conn-id n` argv (the reap-needle
        // distinguisher). Single-conn path: `sidecar_argv(_, None)` == the
        // legacy single-element argv — byte-identical behavior.
        let args = sidecar_argv(&opts.script_path, opts.conn_id);
        let mut cmd = Command::new(&prog);
        cmd.args(&args)
            .env("GROWW_SSM_TOKEN_PARAM", &token_param_path)
            .env("GROWW_TICK_FILE", opts.tick_file.to_string_lossy().as_ref())
            // Connect+subscribe PROOF status file (operator 2026-06-28). The sidecar
            // writes its subscribe counts here atomically; the bridge reads it to emit
            // the ONE CONNECT log + record the counts + flip `connected` on streaming.
            // Counts only — never a credential.
            .env(
                "GROWW_STATUS_FILE",
                opts.status_file.to_string_lossy().as_ref(),
            );
        // §34 auto-scale: fleet children get their conn identity + per-conn
        // watch dir. Counts/paths only — never a credential. Single-conn path
        // sets NONE of these (byte-identical env to pre-scale).
        if let Some(conn_id) = opts.conn_id {
            cmd.env("GROWW_CONN_ID", conn_id.to_string());
        }
        if let Some(watch_dir) = &opts.watch_dir {
            cmd.env("GROWW_WATCH_DIR", watch_dir.to_string_lossy().as_ref());
        }
        if let Some(spec) = &opts.shard_spec {
            cmd.env("GROWW_SHARD_SPEC", spec);
        }
        cmd
            // Capture the child's stdout + stderr so its diagnostic lines (the
            // SDK's bare `Error:`, the SILENT-FEED entitlement line, auth/permission
            // rejects, the redacted traceback) reach `tracing` → CloudWatch +
            // feed_health + Telegram instead of being lost to inherited stdio.
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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

        // Resolve the deferred notifier slot at launch time: by the time a child
        // first prints a reject line, main has stored the live notifier here.
        let notifier_now: Option<Arc<NotificationService>> = notifier.load_full();
        let outcome = supervise_child(
            &mut child,
            Feed::Groww,
            &feed_runtime,
            &feed_health,
            &notifier_now,
            now_ist_nanos_wall,
            &mut storm,
        )
        .await;
        match outcome {
            // Graceful runtime stop → reset the failure curve so re-enabling
            // relaunches immediately.
            SuperviseOutcome::DisabledByToggle => {
                consecutive_failures = 0;
                continue;
            }
            // FEED-STALL-01 (non-storm): the watchdog killed an alive-but-silent
            // child → relaunch IMMEDIATELY (reset the curve) so the feed self-heals
            // within seconds, not at the capped backoff.
            SuperviseOutcome::StallRestart => {
                consecutive_failures = 0;
                continue;
            }
            // A crash/exit OR a stall-restart STORM → normal exponential backoff
            // (the storm path is mapped to Exited so the 60s ceiling bounds a
            // flapping socket, but it NEVER permanently gives up during market
            // hours — the loop keeps retrying).
            SuperviseOutcome::Exited => {}
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

/// Wall-clock current time as IST epoch nanoseconds — the production clock for the
/// stall-watchdog (injected as a fn ptr into `supervise_child` so the pure stall
/// decision stays unit-testable without a live clock). Mirrors the IST-nanos basis
/// the `FeedHealthRegistry` records ticks at, so the age subtraction is consistent.
#[must_use]
fn now_ist_nanos_wall() -> i64 {
    let now_utc_nanos = chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_else(|| chrono::Utc::now().timestamp().saturating_mul(1_000_000_000));
    now_utc_nanos.saturating_add(
        i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS)
            .saturating_mul(1_000_000_000),
    )
}

/// Backoff (seconds) between a supervisor-task death and its respawn — small so
/// the feed is unsupervised only briefly, but non-zero so a tight panic loop can
/// never busy-spin the CPU.
pub const FEED_SUPERVISOR_RESPAWN_BACKOFF_SECS: u64 = 2;

/// Classify how the supervisor task finished, into the FEED-SUPERVISOR-01
/// [`SupervisorJoinOutcome`] used by [`should_respawn_supervisor`]. Pure — kept
/// separate from the live `JoinError` (which has no public constructor) so the
/// respawn decision is unit-testable.
#[must_use]
pub fn classify_supervisor_join(
    join_result: &Result<(), tokio::task::JoinError>,
) -> SupervisorJoinOutcome {
    match join_result {
        // The inner loop is infinite, so a clean `Ok(())` return is unexpected.
        Ok(()) => SupervisorJoinOutcome::Returned,
        Err(e) if e.is_panic() => SupervisorJoinOutcome::Panicked,
        // Cancelled / aborted (runtime shutdown) — a clean teardown.
        Err(_) => SupervisorJoinOutcome::Cancelled,
    }
}

/// FEED-SUPERVISOR-01: spawn [`run_groww_sidecar_supervisor`] under a respawning
/// supervisor so the feed-agnostic stall-watchdog can NEVER die silently. Mirrors
/// the WS-GAP-05 / DISK-WATCHER-01 pattern: on a panic / unexpected return it logs
/// `error!(code=FEED-SUPERVISOR-01)`, increments
/// `tv_feed_supervisor_respawn_total{feed}`, and respawns after a small backoff. A
/// genuine cancel (runtime shutdown) is NOT respawned.
// TEST-EXEMPT: an infinite respawn driver; the pure decisions it relies on (classify_supervisor_join, should_respawn_supervisor) are unit-tested below.
pub fn spawn_supervised_groww_sidecar_supervisor(
    feed_runtime: Arc<FeedRuntimeState>,
    opts: GrowwSidecarOptions,
    feed_health: Arc<FeedHealthRegistry>,
    notifier: Arc<arc_swap::ArcSwapOption<NotificationService>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let handle = tokio::spawn(run_groww_sidecar_supervisor(
                Arc::clone(&feed_runtime),
                opts.clone(),
                Arc::clone(&feed_health),
                Arc::clone(&notifier),
            ));
            let join_result = handle.await;
            let outcome = classify_supervisor_join(&join_result);
            if !should_respawn_supervisor(outcome) {
                // Cancelled / shutdown — clean teardown, do not respawn.
                info!(
                    feed = Feed::Groww.as_str(),
                    "[feeds] sidecar supervisor task ended (shutdown) — not respawning"
                );
                return;
            }
            error!(
                code = ErrorCode::FeedSupervisor01Respawned.code_str(),
                feed = Feed::Groww.as_str(),
                outcome = ?outcome,
                backoff_secs = FEED_SUPERVISOR_RESPAWN_BACKOFF_SECS,
                "[feeds] FEED-SUPERVISOR-01: sidecar supervisor task died — respawning so the \
                 stall-watchdog keeps running (feed self-heal never goes unsupervised)"
            );
            metrics::counter!(
                "tv_feed_supervisor_respawn_total",
                "feed" => Feed::Groww.as_str(),
            )
            .increment(1);
            sleep(Duration::from_secs(FEED_SUPERVISOR_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

/// §34 auto-scale (PR-2 Item 5): the fleet reconciler. Keeps exactly
/// `desired_conns` per-conn sidecar supervision tasks alive — the ladder
/// publishes `desired_conns` (an `Arc<AtomicUsize>`) and pokes `wake`;
/// this loop spawns the missing children (lowest conn id first) and kills
/// the NEWEST children on scale-down (aborting the task drops its `Child`,
/// whose `kill_on_drop(true)` reaps the Python process). A finished child
/// task (panic/return) is respawned with the FEED-SUPERVISOR-01 signal —
/// a fleet child can never die silently.
///
/// Pure decision math (`fleet_reconcile_actions`) is unit-tested below;
/// this loop is composition-only.
// TEST-EXEMPT: spawn/abort/sleep reconciler loop; the pure reconcile decision (fleet_reconcile_actions) + the per-conn option/env/argv/needle primitives are unit-tested, and the child lifecycle is the already-tested run_groww_sidecar_supervisor.
pub fn spawn_groww_scale_fleet(
    feed_runtime: Arc<FeedRuntimeState>,
    base_opts: GrowwSidecarOptions,
    shards_root: PathBuf,
    desired_conns: Arc<std::sync::atomic::AtomicUsize>,
    wake: Arc<tokio::sync::Notify>,
    feed_health: Arc<FeedHealthRegistry>,
    notifier: Arc<arc_swap::ArcSwapOption<NotificationService>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut children: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        loop {
            let want = desired_conns.load(std::sync::atomic::Ordering::Relaxed);
            // Respawn any finished child in place (FEED-SUPERVISOR-01).
            for (conn_id, handle) in children.iter_mut().enumerate() {
                if handle.is_finished() {
                    error!(
                        code = ErrorCode::FeedSupervisor01Respawned.code_str(),
                        feed = Feed::Groww.as_str(),
                        conn_id,
                        "[feeds] FEED-SUPERVISOR-01: fleet child supervision task died — respawning"
                    );
                    metrics::counter!(
                        "tv_feed_supervisor_respawn_total",
                        "feed" => Feed::Groww.as_str(),
                        "component" => "scale_fleet_child",
                    )
                    .increment(1);
                    *handle = spawn_fleet_child(
                        &feed_runtime,
                        &base_opts,
                        &shards_root,
                        conn_id,
                        &feed_health,
                        &notifier,
                    );
                }
            }
            match fleet_reconcile_actions(children.len(), want) {
                FleetAction::SpawnUpTo(target) => {
                    while children.len() < target {
                        let conn_id = children.len();
                        info!(conn_id, "[feeds] groww scale fleet: spawning connection");
                        children.push(spawn_fleet_child(
                            &feed_runtime,
                            &base_opts,
                            &shards_root,
                            conn_id,
                            &feed_health,
                            &notifier,
                        ));
                    }
                }
                FleetAction::KillNewestTo(target) => {
                    while children.len() > target {
                        let conn_id = children.len() - 1;
                        info!(
                            conn_id,
                            "[feeds] groww scale fleet: killing NEWEST connection (scale-down)"
                        );
                        if let Some(handle) = children.pop() {
                            // Abort drops the supervise future → drops the
                            // Child → kill_on_drop reaps the Python process.
                            handle.abort();
                        }
                    }
                }
                FleetAction::Steady => {}
            }
            metrics::gauge!("tv_groww_conns_active").set(children.len() as f64);
            tokio::select! {
                () = wake.notified() => {}
                () = sleep(SIDECAR_DISABLE_POLL) => {}
            }
        }
    })
}

/// Spawn one fleet child's per-conn supervision loop.
// TEST-EXEMPT: thin spawn wrapper over the tested run_groww_sidecar_supervisor + pure shard option derivation.
fn spawn_fleet_child(
    feed_runtime: &Arc<FeedRuntimeState>,
    base_opts: &GrowwSidecarOptions,
    shards_root: &Path,
    conn_id: usize,
    feed_health: &Arc<FeedHealthRegistry>,
    notifier: &Arc<arc_swap::ArcSwapOption<NotificationService>>,
) -> tokio::task::JoinHandle<()> {
    let files = crate::groww_bridge::shard_files(shards_root, conn_id);
    let opts = shard_sidecar_options(base_opts, &files, format!("c{conn_id:02}"));
    tokio::spawn(run_groww_sidecar_supervisor(
        Arc::clone(feed_runtime),
        opts,
        Arc::clone(feed_health),
        Arc::clone(notifier),
    ))
}

/// §34: pure fleet reconcile decision — spawn missing (lowest ids first),
/// kill newest on scale-down, steady otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FleetAction {
    /// Spawn children until the fleet reaches this size.
    SpawnUpTo(usize),
    /// Abort newest children until the fleet shrinks to this size.
    KillNewestTo(usize),
    /// Fleet already matches the desired size.
    Steady,
}

/// Pure reconcile decision for the fleet loop.
#[must_use]
pub const fn fleet_reconcile_actions(current: usize, desired: usize) -> FleetAction {
    if current < desired {
        FleetAction::SpawnUpTo(desired)
    } else if current > desired {
        FleetAction::KillNewestTo(desired)
    } else {
        FleetAction::Steady
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

    // ── FEED-STALL-01: the feed-agnostic stall decision ──────────────────────

    #[test]
    fn test_should_restart_on_stall_true_when_stale_market_open_enabled() {
        // Alive but silent across the whole universe past the threshold, during
        // market hours, feed enabled → restart (today's 10:31 dead-socket case).
        assert!(should_restart_on_stall(
            Some(FEED_STALL_RESTART_SECS + 1),
            true,
            true,
            FEED_STALL_RESTART_SECS
        ));
    }

    #[test]
    fn test_should_restart_on_stall_false_off_hours() {
        // Off-hours silence is NORMAL (don't regress #1261 idle-is-normal); even a
        // huge gap must NOT restart when the market is closed (covers the 15:30:00
        // close boundary: market_open=false → no restart).
        assert!(!should_restart_on_stall(
            Some(FEED_STALL_RESTART_SECS + 100),
            false,
            true,
            FEED_STALL_RESTART_SECS
        ));
    }

    #[test]
    fn test_should_restart_on_stall_false_disabled() {
        // Operator disabled the feed mid-stall → the disable path wins; the
        // watchdog must NOT fight the toggle.
        assert!(!should_restart_on_stall(
            Some(FEED_STALL_RESTART_SECS + 100),
            true,
            false,
            FEED_STALL_RESTART_SECS
        ));
    }

    #[test]
    fn test_should_restart_on_stall_false_fresh() {
        // Ticks flowing recently → never restart.
        assert!(!should_restart_on_stall(
            Some(2),
            true,
            true,
            FEED_STALL_RESTART_SECS
        ));
    }

    #[test]
    fn test_should_restart_on_stall_false_no_tick_yet() {
        // No first tick yet (cold pre-open) → None → never restart. A feed that has
        // not streamed its first tick is the silent-feed diagnostic's job, not a
        // kill loop.
        assert!(!should_restart_on_stall(
            None,
            true,
            true,
            FEED_STALL_RESTART_SECS
        ));
    }

    #[test]
    fn test_should_restart_on_stall_boundary_exactly_at_threshold() {
        // Strict `>`: exactly AT the threshold must NOT fire (one extra second does).
        assert!(!should_restart_on_stall(
            Some(FEED_STALL_RESTART_SECS),
            true,
            true,
            FEED_STALL_RESTART_SECS
        ));
        assert!(should_restart_on_stall(
            Some(FEED_STALL_RESTART_SECS + 1),
            true,
            true,
            FEED_STALL_RESTART_SECS
        ));
    }

    #[test]
    fn test_should_restart_on_stall_is_feed_agnostic_for_novel_feed() {
        // GENERALITY PROOF: the decision fn takes NO Feed argument — it operates
        // purely on (age, market_open, enabled, threshold). The SAME call therefore
        // governs EVERY current and FUTURE feed identically; a new feed #3…N
        // inherits the identical stall→restart with ZERO new code. We assert the
        // identical inputs yield the identical verdict regardless of which feed's
        // age was fed in (the inputs are feed-independent by construction).
        let stale = Some(FEED_STALL_RESTART_SECS + 5);
        let verdict = should_restart_on_stall(stale, true, true, FEED_STALL_RESTART_SECS);
        // Re-run for every existing feed's "age" (same number) — same verdict.
        for &_feed in tickvault_common::feed::Feed::ALL {
            assert_eq!(
                should_restart_on_stall(stale, true, true, FEED_STALL_RESTART_SECS),
                verdict
            );
        }
        assert!(
            verdict,
            "a stale feed-level gap restarts ANY feed identically"
        );
    }

    #[test]
    fn test_stall_restart_storm_is_bounded() {
        // A flapping socket (reconnect→re-drop repeatedly): the first
        // STALL_RESTART_STORM_MAX restarts inside the window are NOT a storm; the
        // next one OVER the max IS a storm (escalation edge). Never gives up — it
        // just flips to the ceiling path.
        let mut storm = StallRestartStorm::default();
        let t0 = 1_000u64;
        for i in 0..STALL_RESTART_STORM_MAX {
            assert!(
                !storm.record_and_is_storm(t0 + u64::from(i)),
                "restart {i} within the cap is not yet a storm"
            );
        }
        // One more inside the window → over the cap → storm.
        assert!(storm.record_and_is_storm(t0 + u64::from(STALL_RESTART_STORM_MAX)));
        assert!(storm.count_in_window() > STALL_RESTART_STORM_MAX);

        // After the window elapses, the counter resets — a later isolated restart
        // is NOT a storm (the feed recovered then stalled once much later).
        let mut storm2 = StallRestartStorm::default();
        assert!(!storm2.record_and_is_storm(t0));
        assert!(!storm2.record_and_is_storm(t0 + STALL_RESTART_STORM_WINDOW_SECS + 10));
        assert_eq!(
            storm2.count_in_window(),
            1,
            "window reset to a single restart"
        );
    }

    #[test]
    fn test_record_and_is_storm_and_count_in_window_track_the_window() {
        // Explicit coverage of StallRestartStorm::record_and_is_storm +
        // count_in_window (the pub fns) by name.
        let mut storm = StallRestartStorm::default();
        assert_eq!(storm.count_in_window(), 0);
        assert!(!storm.record_and_is_storm(100));
        assert_eq!(storm.count_in_window(), 1);
        assert!(!storm.record_and_is_storm(101));
        assert_eq!(storm.count_in_window(), 2);
    }

    #[test]
    fn test_stall_restart_storm_tolerates_backward_clock_step() {
        // A backward clock step (now_secs < window_start) must not panic or
        // false-reset; saturating_sub yields 0 (inside window) → counts normally.
        let mut storm = StallRestartStorm::default();
        assert!(!storm.record_and_is_storm(1_000));
        assert!(!storm.record_and_is_storm(900)); // backward step
        assert_eq!(storm.count_in_window(), 2);
    }

    // ── FEED-SUPERVISOR-01: respawn decision ─────────────────────────────────

    #[test]
    fn test_should_respawn_supervisor_on_panic_or_return() {
        assert!(should_respawn_supervisor(SupervisorJoinOutcome::Panicked));
        assert!(should_respawn_supervisor(SupervisorJoinOutcome::Returned));
        // A genuine cancel (runtime shutdown) is a clean teardown — do NOT respawn.
        assert!(!should_respawn_supervisor(SupervisorJoinOutcome::Cancelled));
    }

    #[tokio::test]
    async fn test_classify_supervisor_join_maps_panic_and_cancel() {
        // A panicking task → JoinError where is_panic() → Panicked.
        let panic_handle = tokio::spawn(async { panic!("boom") });
        let panic_join = panic_handle.await;
        assert_eq!(
            classify_supervisor_join(&panic_join.map(|_| ())),
            SupervisorJoinOutcome::Panicked
        );
        // An aborted task → JoinError where is_cancelled() → Cancelled.
        let abort_handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        abort_handle.abort();
        let abort_join = abort_handle.await;
        assert_eq!(
            classify_supervisor_join(&abort_join.map(|_| ())),
            SupervisorJoinOutcome::Cancelled
        );
    }

    #[test]
    fn test_stall_restart_reuses_existing_relaunch_path() {
        // SEAM GUARD: the stall path must reuse the EXISTING relaunch loop — i.e.
        // supervise_child returns SuperviseOutcome::{StallRestart|Exited} and the
        // top-level run_groww_sidecar_supervisor loop relaunches via the SAME
        // sidecar_restart_backoff curve. A refactor that bypassed the existing loop
        // (e.g. relaunching inline inside supervise_child) would break this. We pin
        // the source so the seam can't silently break.
        let src = include_str!("groww_sidecar_supervisor.rs");
        assert!(
            src.contains("SuperviseOutcome::StallRestart"),
            "the stall arm must return through the SuperviseOutcome enum (existing relaunch loop)"
        );
        assert!(
            src.contains("now_ist_nanos_wall")
                && src.contains("supervise_child(")
                && src.contains("sidecar_restart_backoff(consecutive_failures)"),
            "the relaunch must flow through the existing run_groww_sidecar_supervisor backoff loop"
        );
    }

    #[test]
    fn test_python_sidecar_active_self_heal_is_wired() {
        // L1 SEAM GUARD: the Python sidecar must (a) actively force-close the NATS
        // socket on a market-hours stall (so the blocking consume() returns and the
        // except→reconnect re-subscribes), (b) use the ms reconnect ladder during
        // market hours, and (c) carry the pure-decision selftest. A refactor that
        // silently reverted to print-only watchdog or dropped the ms ladder breaks
        // this. Source-scanned so `cargo test` covers the L1 wiring (the Python
        // unit assertions run under `groww_sidecar.py --selftest`).
        let py = include_str!("../../../scripts/groww-sidecar/groww_sidecar.py");
        assert!(
            py.contains("_force_close_nats_socket"),
            "the sidecar must force-close the NATS socket to break the blocking consume()"
        );
        assert!(
            py.contains("def should_force_reconnect"),
            "the sidecar must carry the pure force-reconnect decision fn"
        );
        assert!(
            py.contains("def _is_within_market_hours_ist"),
            "the sidecar must gate the ms reconnect ladder + force-reconnect on market hours"
        );
        assert!(
            py.contains("MARKET_OPEN_RECONNECT_BASE_SECS"),
            "the sidecar must use the fast ms reconnect ladder during market hours"
        );
        assert!(
            py.contains("def _stall_recovery_loop"),
            "the sidecar must run an active stall-recovery loop after data starts flowing"
        );
        assert!(
            py.contains("_selftest_self_heal"),
            "the sidecar must carry the self-heal selftest (run under --selftest)"
        );
        // Rate-limit ladder is preserved (a 429 must NOT use the ms ladder).
        assert!(
            py.contains("RATE_LIMIT_BACKOFF_BASE_SECS"),
            "the sidecar must keep the 60s rate-limit ladder even in market hours"
        );
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
        assert_eq!(
            opts.status_file,
            PathBuf::from(crate::groww_bridge::GROWW_STATUS_FILE_DEFAULT)
        );
    }

    #[test]
    fn test_supervisor_injects_status_file_env() {
        // Connect+subscribe PROOF (2026-06-28): the supervisor MUST inject
        // GROWW_STATUS_FILE into the child so the sidecar writes its subscribe
        // counts where the bridge reads them. `run_groww_sidecar_supervisor` is a
        // TEST-EXEMPT process driver, so pin the injection by source-scan — a
        // future refactor cannot silently drop it.
        let src = include_str!("groww_sidecar_supervisor.rs");
        // Scan the env-var KEY literal + the `opts.status_file` value source
        // independently — rustfmt may wrap the `.env("GROWW_STATUS_FILE", ...)` call
        // across lines, so a contiguous-needle scan is brittle. Both tokens present
        // ⇒ the injection is wired.
        let env_key = format!("\"GROWW_STATUS{}_FILE\"", "");
        assert!(
            src.contains(&env_key) && src.contains("opts.status_file.to_string_lossy"),
            "the supervisor must inject GROWW_STATUS_FILE (the connect+subscribe \
             proof status file) into the sidecar child"
        );
    }

    #[test]
    fn test_supervisor_injects_token_param_env_and_no_credentials() {
        // Shared token-minter lock 2026-07-02: the supervisor hands the child
        // ONLY the SSM parameter PATH of the minter-written access token —
        // never a credential. Source-scan (the supervise loop is TEST-EXEMPT).
        let src = include_str!("groww_sidecar_supervisor.rs");
        let token_key = format!("\"GROWW_SSM{}_TOKEN_PARAM\"", "");
        assert!(
            src.contains(&token_key) && src.contains("GROWW_ACCESS_TOKEN_SECRET"),
            "the supervisor must inject GROWW_SSM_TOKEN_PARAM (built from \
             GROWW_ACCESS_TOKEN_SECRET) into the sidecar child"
        );
        // The former credential env injections must never return.
        for banned in [
            format!("\"GROWW_API{}_KEY\"", ""),
            format!("\"GROWW_TOTP{}_SECRET\"", ""),
        ] {
            assert!(
                !src.contains(&banned),
                "token-minter lock violated: the supervisor injects the {banned} \
                 credential env var — Groww credentials are Lambda-only"
            );
        }
    }

    // ── requirements fingerprint re-provisioning gate (2026-07-02 boto3 swap) ──

    #[test]
    fn test_requirements_fingerprint_is_stable_and_content_sensitive() {
        let a = requirements_fingerprint("growwapi==1.5.0\nboto3==1.35.36\n");
        let b = requirements_fingerprint("growwapi==1.5.0\nboto3==1.35.36\n");
        let c = requirements_fingerprint("growwapi==1.5.0\npyotp==2.9.0\n");
        assert_eq!(a, b, "same contents must fingerprint identically");
        assert_ne!(a, c, "changed contents must change the fingerprint");
        assert_eq!(a.len(), 16, "fnv-1a 64-bit hex is 16 chars");
    }

    #[test]
    fn test_venv_needs_provisioning_on_missing_python_or_changed_deps() {
        // Fresh box: python absent → provision regardless of marker.
        assert!(venv_needs_provisioning(false, None, "abc"));
        assert!(venv_needs_provisioning(false, Some("abc"), "abc"));
        // Deployed box, unchanged deps → skip.
        assert!(!venv_needs_provisioning(true, Some("abc"), "abc"));
        // Deployed box, CHANGED deps (the pyotp→boto3 swap) → re-provision.
        assert!(venv_needs_provisioning(true, Some("old"), "new"));
        // Deployed box, marker missing (pre-marker deployment) → re-provision
        // once (idempotent pip) so the marker gets written.
        assert!(venv_needs_provisioning(true, None, "abc"));
    }

    #[test]
    fn test_requirements_marker_path_is_inside_the_venv() {
        let p = requirements_marker_path(Path::new("data/groww/venv"));
        assert_eq!(
            p,
            PathBuf::from("data/groww/venv/.requirements-fingerprint")
        );
    }

    #[test]
    fn test_classify_access_token_stale_marker_is_auth_rejected() {
        // 2026-07-02 adversarial finding H1: the sidecar's 10-min minter-dead
        // marker MUST route to feed-health Down + Telegram (AuthRejected), not
        // classify Info and vanish.
        let line = "GROWW LIVE FEED REJECTED: access token stale for 612s (>10min) — \
                    the bruteX groww-token-minter Lambda may not have run";
        assert_eq!(classify_sidecar_line(line), SidecarLineClass::AuthRejected);
        assert!(classify_sidecar_line(line).sets_auth_rejected());
    }

    // ── is_silent_feed_diagnostic + silent_feed_diagnostic_level (the
    // market-hours-aware log re-leveling for the benign-after-close SILENT-FEED
    // watchdog lines — operator log-flood fix 2026-06-29) ──

    #[test]
    fn test_is_silent_feed_diagnostic_matches_watchdog_lines() {
        // The two real watchdog strings the sidecar prints every 30s.
        assert!(is_silent_feed_diagnostic(
            "groww sidecar: SILENT FEED — subscribed 742 stocks + 25 indices but received NO live records in 30s"
        ));
        assert!(is_silent_feed_diagnostic(
            "groww sidecar: STILL SILENT — 0 live records decoded (emitted=0, dropped=0)"
        ));
        // Case-insensitive.
        assert!(is_silent_feed_diagnostic("silent feed warning"));
    }

    #[test]
    fn test_is_silent_feed_diagnostic_excludes_genuine_faults() {
        // A hard entitlement / permission / authorization line is a real config
        // fault at ANY time — NOT a silent-feed watchdog line, so it keeps its
        // error level always.
        assert!(!is_silent_feed_diagnostic(
            "this Groww account lacks a LIVE market-data feed entitlement"
        ));
        assert!(!is_silent_feed_diagnostic(
            "NATS permissions violation for subscription"
        ));
        assert!(!is_silent_feed_diagnostic("not authorized to subscribe"));
        assert!(!is_silent_feed_diagnostic(
            "groww sidecar error [auth]: bad token"
        ));
        assert!(!is_silent_feed_diagnostic("Error: nats: connection closed"));
        assert!(!is_silent_feed_diagnostic(""));
    }

    #[test]
    fn test_silent_feed_diagnostic_level_market_closed_is_info_not_error() {
        // The fix: market CLOSED → INFO (calm "idle" note, NOT an ERROR that
        // floods errors.jsonl / errors.log every 60s after close).
        assert_eq!(
            silent_feed_diagnostic_level(false),
            tracing::Level::INFO,
            "after-hours SILENT-FEED idle must be INFO, never ERROR"
        );
    }

    #[test]
    fn test_silent_feed_diagnostic_level_market_open_is_error() {
        // During market hours prolonged silence IS abnormal → keep ERROR so the
        // operator is alerted to a real entitlement gap / reject.
        assert_eq!(
            silent_feed_diagnostic_level(true),
            tracing::Level::ERROR,
            "during market hours a silent feed must stay ERROR"
        );
    }

    // ── classify_sidecar_line (the pure diagnostic classifier) ──
    // pub-fn-test-guard one-liner: these tests exercise classify_sidecar_line +
    // SidecarLineClass triggers_alert + sets_auth_rejected + clears_auth_rejected + alert_reason across every class below.

    #[test]
    fn test_classify_silent_feed_and_entitlement_are_entitlement_rejected() {
        // The sidecar's SILENT-FEED watchdog line + its "account lacks … live
        // market-data feed entitlement" phrasing → EntitlementRejected.
        assert_eq!(
            classify_sidecar_line(
                "groww sidecar: SILENT FEED — subscribed 765 stocks + 2 indices but received NO live records"
            ),
            SidecarLineClass::EntitlementRejected
        );
        assert_eq!(
            classify_sidecar_line("groww sidecar: STILL SILENT — 0 live records decoded"),
            SidecarLineClass::EntitlementRejected
        );
        assert_eq!(
            classify_sidecar_line(
                "this Groww account lacks a LIVE market-data feed entitlement (socket connects but streams nothing)"
            ),
            SidecarLineClass::EntitlementRejected
        );
    }

    #[test]
    fn test_classify_permissions_and_authorization_are_entitlement_rejected() {
        // The SDK's swallowed NATS permissions/authorization wording.
        assert_eq!(
            classify_sidecar_line("NATS permissions violation for subscription"),
            SidecarLineClass::EntitlementRejected
        );
        assert_eq!(
            classify_sidecar_line("not authorized to subscribe to this subject"),
            SidecarLineClass::EntitlementRejected
        );
    }

    #[test]
    fn test_classify_auth_phase_error_is_auth_rejected() {
        // The auth-phase reject is the most specific cause — beats the generic Error.
        assert_eq!(
            classify_sidecar_line(
                "groww sidecar error [auth]: ApiException: Token key not found or inactive — reconnecting in 10s"
            ),
            SidecarLineClass::AuthRejected
        );
    }

    #[test]
    fn test_classify_bare_error_and_other_phases_are_error() {
        // The SDK's bare "Error:" NATS line (its swallowed per-frame errors).
        assert_eq!(classify_sidecar_line("Error:"), SidecarLineClass::Error);
        assert_eq!(
            classify_sidecar_line("Error: nats: connection closed"),
            SidecarLineClass::Error
        );
        // Other phase rejects (feed-connect / subscribe / consume) → Error.
        assert_eq!(
            classify_sidecar_line(
                "groww sidecar error [feed-connect]: ConnectionError: refused — reconnecting in 4s"
            ),
            SidecarLineClass::Error
        );
        assert_eq!(
            classify_sidecar_line("groww sidecar error [consume] traceback (failure #1):"),
            SidecarLineClass::Error
        );
    }

    #[test]
    fn test_classify_subscribed_line() {
        assert_eq!(
            classify_sidecar_line("subscribed 765 stocks + 2 indices — awaiting first tick…"),
            SidecarLineClass::Subscribed
        );
    }

    #[test]
    fn test_classify_positive_and_info_lines() {
        assert_eq!(
            classify_sidecar_line("groww auth OK: access token acquired (len=512)"),
            SidecarLineClass::Streaming
        );
        assert_eq!(
            classify_sidecar_line("groww sidecar → appending NDJSON to data/groww/ticks.ndjson"),
            SidecarLineClass::Streaming
        );
        // A real LTP/NDJSON data line / anything else → Info (no side-effect).
        assert_eq!(
            classify_sidecar_line(r#"{"sid":1333,"ltp":2847.5,"ts":1780000000123}"#),
            SidecarLineClass::Info
        );
        assert_eq!(classify_sidecar_line(""), SidecarLineClass::Info);
    }

    #[test]
    fn test_classify_is_case_insensitive() {
        assert_eq!(
            classify_sidecar_line("SILENT feed warning"),
            SidecarLineClass::EntitlementRejected
        );
        assert_eq!(
            classify_sidecar_line("GROWW SIDECAR ERROR [AUTH]: bad token"),
            SidecarLineClass::AuthRejected
        );
    }

    #[test]
    fn test_sidecar_line_class_triggers_alert_only_for_reject_classes() {
        assert!(SidecarLineClass::EntitlementRejected.triggers_alert());
        assert!(SidecarLineClass::AuthRejected.triggers_alert());
        assert!(SidecarLineClass::Error.triggers_alert());
        assert!(!SidecarLineClass::Subscribed.triggers_alert());
        assert!(!SidecarLineClass::Streaming.triggers_alert());
        assert!(!SidecarLineClass::Info.triggers_alert());
    }

    #[test]
    fn test_sidecar_line_class_sets_auth_rejected_only_for_hard_reject_classes() {
        // MEDIUM (root) fix: the actionable auth-rejected RED ("refresh the SSM
        // api-key") must latch ONLY for a CONFIRMED auth / entitlement reject —
        // NEVER for the generic transient `Error` class (a NATS/SDK reconnect or a
        // recovered traceback). This is strictly narrower than triggers_alert.
        assert!(SidecarLineClass::AuthRejected.sets_auth_rejected());
        assert!(SidecarLineClass::EntitlementRejected.sets_auth_rejected());
        assert!(
            !SidecarLineClass::Error.sets_auth_rejected(),
            "the generic Error class must NOT latch the false 'refresh the api-key' RED"
        );
        assert!(!SidecarLineClass::Subscribed.sets_auth_rejected());
        assert!(!SidecarLineClass::Streaming.sets_auth_rejected());
        assert!(!SidecarLineClass::Info.sets_auth_rejected());
        // sets_auth_rejected ⊆ triggers_alert (a class that latches the RED must
        // also alert; but not every alert class latches the RED — that's Error).
        for class in [
            SidecarLineClass::AuthRejected,
            SidecarLineClass::EntitlementRejected,
            SidecarLineClass::Error,
            SidecarLineClass::Subscribed,
            SidecarLineClass::Streaming,
            SidecarLineClass::Info,
        ] {
            if class.sets_auth_rejected() {
                assert!(
                    class.triggers_alert(),
                    "any auth-latching class must also be an alert class: {class:?}"
                );
            }
        }
    }

    #[test]
    fn test_error_class_drives_an_alert_without_latching_auth_rejected() {
        // The exact MEDIUM scenario: a benign NATS reconnect / recovered
        // `error:` line classifies as Error → it IS an alert (visibility) but is
        // NOT an auth latch (no false "refresh the api-key" Down).
        let class = classify_sidecar_line("Error: nats connection reset, reconnecting");
        assert_eq!(class, SidecarLineClass::Error);
        assert!(class.triggers_alert(), "Error keeps its visibility alert");
        assert!(
            !class.sets_auth_rejected(),
            "Error must not latch the actionable auth-rejected RED"
        );
    }

    #[test]
    fn test_sidecar_line_class_clears_auth_rejected_only_for_streaming() {
        // HIGH-2: a confirmed streaming / auth-OK line is the NON-TICK recovery
        // edge that clears a stale auth_rejected. Only `Streaming` qualifies —
        // `Subscribed` is a one-shot startup line, not proof the stream recovered.
        assert!(SidecarLineClass::Streaming.clears_auth_rejected());
        assert!(!SidecarLineClass::Subscribed.clears_auth_rejected());
        assert!(!SidecarLineClass::AuthRejected.clears_auth_rejected());
        assert!(!SidecarLineClass::EntitlementRejected.clears_auth_rejected());
        assert!(!SidecarLineClass::Error.clears_auth_rejected());
        assert!(!SidecarLineClass::Info.clears_auth_rejected());
        // A real `groww auth OK` line classifies as Streaming → clears.
        assert!(classify_sidecar_line("groww auth OK").clears_auth_rejected());
    }

    #[test]
    fn test_alert_reason_present_only_for_alert_classes_and_is_plain_english() {
        // Every alert class has a fixed plain-English reason; non-alert classes
        // have none. The reasons carry no library names / file paths / raw input.
        for class in [
            SidecarLineClass::EntitlementRejected,
            SidecarLineClass::AuthRejected,
            SidecarLineClass::Error,
        ] {
            let reason = class
                .alert_reason()
                .expect("alert class must have a reason");
            assert!(!reason.is_empty());
            assert!(!reason.contains(".rs"), "file path in reason: {reason}");
            assert!(!reason.contains("Stdio"), "lib jargon in reason: {reason}");
        }
        assert!(SidecarLineClass::Subscribed.alert_reason().is_none());
        assert!(SidecarLineClass::Streaming.alert_reason().is_none());
        assert!(SidecarLineClass::Info.alert_reason().is_none());
    }

    #[test]
    fn test_supervisor_pipes_and_drains_child_stdio() {
        // The verified bug fix: the supervisor MUST pipe stdout + stderr and drain
        // them (so the sidecar's diagnostic lines reach tracing + feed_health +
        // Telegram instead of inherited stdio). The supervise loop is a TEST-EXEMPT
        // process driver, so pin the wiring by source-scan — a future refactor
        // cannot silently restore the inherited-stdio blindness.
        let src = include_str!("groww_sidecar_supervisor.rs");
        assert!(
            src.contains("Stdio::piped()"),
            "the supervisor must pipe the child's stdout + stderr"
        );
        assert!(
            src.contains(".stdout(Stdio::piped())") && src.contains(".stderr(Stdio::piped())"),
            "BOTH stdout and stderr must be piped"
        );
        assert!(
            src.contains("spawn_pipe_drain")
                && src.contains("child.stdout.take()")
                && src.contains("child.stderr.take()"),
            "the supervisor must drain both captured pipes"
        );
        assert!(
            src.contains("feed_health.set_auth_rejected(Feed::Groww, true)")
                && src.contains("NotificationEvent::GrowwSidecarRejected"),
            "an alert-class line must mark feed_health rejected + fire the typed Telegram event"
        );
    }

    // ── §34 auto-scale (PR-2 Item 5) ──

    /// Plan test: fleet children carry conn identity + shard spec through
    /// options AND the child env injection exists in the launch source.
    #[test]
    fn test_shard_sidecar_options_env_carries_conn_id_and_spec() {
        let base = GrowwSidecarOptions::default();
        let files = crate::groww_bridge::shard_files(Path::new("data/groww/shards"), 3);
        let opts = shard_sidecar_options(&base, &files, "c03".to_string());
        assert_eq!(opts.conn_id, Some(3));
        assert_eq!(opts.shard_spec.as_deref(), Some("c03"));
        assert_eq!(opts.watch_dir, Some(files.watch_dir.clone()));
        assert_eq!(opts.tick_file, files.tick_file);
        assert_eq!(opts.status_file, files.status_file);
        // Shared provision surface (one venv, N children).
        assert_eq!(opts.venv_dir, base.venv_dir);
        assert_eq!(opts.script_path, base.script_path);
        // Source-scan: the launch site injects the 3 per-conn env vars
        // (mirrors the GROWW_STATUS_FILE wiring ratchet above).
        let src = include_str!("groww_sidecar_supervisor.rs");
        for needle in [
            "cmd.env(\"GROWW_CONN_ID\"",
            "cmd.env(\"GROWW_WATCH_DIR\"",
            "cmd.env(\"GROWW_SHARD_SPEC\"",
        ] {
            assert!(src.contains(needle), "launch site must inject {needle}");
        }
    }

    /// Ratchet: scale disabled (the default `conn_id: None`) is byte-identical
    /// to the pre-scale child contract — same argv, same reap needle, no
    /// per-conn env values in the options.
    #[test]
    fn test_single_conn_path_unchanged_when_scale_disabled() {
        let opts = GrowwSidecarOptions::default();
        assert_eq!(opts.conn_id, None);
        assert_eq!(opts.watch_dir, None);
        assert_eq!(opts.shard_spec, None);
        let script = Path::new("scripts/groww-sidecar/groww_sidecar.py");
        // argv: exactly the script (what build_sidecar_run_command produced).
        assert_eq!(
            sidecar_argv(script, None),
            vec![script.to_string_lossy().into_owned()]
        );
        // reap needle: exactly the script path (legacy behavior).
        assert_eq!(
            orphan_reap_needle(script, None),
            script.to_string_lossy().into_owned()
        );
    }

    /// §34: per-conn needle is anchored so conn 3 never matches conn 30.
    #[test]
    fn test_orphan_reap_needle_is_per_conn_anchored() {
        let script = Path::new("scripts/groww-sidecar/groww_sidecar.py");
        let n3 = orphan_reap_needle(script, Some(3));
        assert!(n3.ends_with("--conn-id 3$"), "{n3}");
        let n30 = orphan_reap_needle(script, Some(30));
        assert!(n30.ends_with("--conn-id 30$"));
        assert_ne!(n3, n30);
        // The conn-3 anchored regex cannot match a conn-30 cmdline.
        let conn30_cmdline = format!("{} --conn-id 30", script.to_string_lossy());
        assert!(
            !conn30_cmdline.ends_with("--conn-id 3"),
            "anchor semantics: conn-30 cmdline must not end with the conn-3 suffix"
        );
    }

    /// §34: fleet argv carries the conn id flag pair.
    #[test]
    fn test_sidecar_argv_fleet_child_has_conn_id_flag() {
        let script = Path::new("scripts/groww-sidecar/groww_sidecar.py");
        let argv = sidecar_argv(script, Some(7));
        assert_eq!(argv.len(), 3);
        assert_eq!(argv[1], "--conn-id");
        assert_eq!(argv[2], "7");
    }

    /// §34: pure fleet reconcile decision covers all three arms.
    #[test]
    fn test_fleet_reconcile_actions() {
        assert_eq!(fleet_reconcile_actions(0, 2), FleetAction::SpawnUpTo(2));
        assert_eq!(fleet_reconcile_actions(5, 2), FleetAction::KillNewestTo(2));
        assert_eq!(fleet_reconcile_actions(2, 2), FleetAction::Steady);
        assert_eq!(fleet_reconcile_actions(0, 0), FleetAction::Steady);
    }
}
