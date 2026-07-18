//! Phase 0 Item 19 — Dual-instance lock primitives (SSM-backed).
//!
//! The dual-instance lock prevents two `tickvault` processes from running
//! against the same Dhan client-id at the same time. The downstream
//! consequences of a second instance starting under the same JWT are
//! severe:
//!
//!   * The static-IP enforcement (effective 2026-04-01) bans a second
//!     instance from placing orders, but the SECOND instance is the one
//!     Dhan keeps connected — the original instance loses its WebSocket
//!     sessions when the duplicate auth arrives.
//!   * Two instances each compute their own depth/order state, so state
//!     machines disagree, P&L reconciliation breaks, audit fragments.
//!   * The 24h JWT cycle gets corrupted because both processes try to
//!     refresh, racing each other into DH-901 invalidations.
//!
//! **Backend (operator lock 2026-05-24): AWS SSM Parameter Store.**
//! Earlier revisions used Valkey (`SET NX EX`). After the CloudWatch-only
//! migration (#O1) the operator chose SSM so the source-of-truth lives in
//! the same AWS account that runs the EC2 instance — guaranteeing "AWS up
//! → Mac dev cannot accidentally start a duplicate" without keeping a
//! Valkey container alive.
//!
//! Parameter path: `/tickvault/<env>/instance-lock` (plain String — this
//! is operational data, not a secret).
//!
//! Value shape (JSON):
//! ```json
//! { "host_id": "...", "started_at_unix": 1700000000, "last_heartbeat_unix": 1700000030 }
//! ```
//!
//! Acquire semantics:
//!   * `PutParameter` with `overwrite=false` is the atomic "claim" —
//!     SSM rejects the request if the parameter already exists.
//!   * On rejection we `GetParameter`, parse the JSON, and check the
//!     holder's `last_heartbeat_unix`. If older than
//!     `INSTANCE_LOCK_TTL_SECS` the slot is considered stale and we
//!     take over via `PutParameter(overwrite=true)`. Otherwise we
//!     return `AlreadyHeld { holder }`.
//!   * Heartbeat: every 30s the owner re-issues `PutParameter` with a
//!     refreshed `last_heartbeat_unix`. Two missed renewals still leave
//!     30s of headroom before another instance considers the slot stale.
//!   * Release: `GetParameter` + ownership check + `DeleteParameter`.
//!     A foreign holder is left alone (logged at WARN, not ERROR).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow};
use aws_sdk_ssm::Client as SsmClient;
use aws_sdk_ssm::types::ParameterType;
use serde::{Deserialize, Serialize};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::sanitize_ilp_symbol;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// TTL on the lock value. 90 seconds is long enough that a brief network
/// blip won't drop the lock and short enough that a hard crash only
/// blocks recovery for ~90 s.
pub const INSTANCE_LOCK_TTL_SECS: u64 = 90;

/// Heartbeat interval — 1/3 of the TTL. Two missed renewals still leave
/// 30 seconds of headroom before the lock is considered stale.
pub const INSTANCE_LOCK_HEARTBEAT_INTERVAL_SECS: u64 = 30;

/// SSM Parameter path prefix. Full path is `{prefix}/{env}/instance-lock`
/// so dev / sandbox / prod cannot stomp on each other.
pub const INSTANCE_LOCK_SSM_PATH_PREFIX: &str = "/tickvault";

/// The Dhan-session dual-instance lock name (the original Phase 0 Item 19
/// lock). Kept as a named constant so the generic named-lock knob below can
/// prove the legacy path stays byte-identical.
pub const DHAN_INSTANCE_LOCK_NAME: &str = "instance-lock";

/// The Groww scale-FLEET dual-instance lock name (Session-B fix, operator go
/// 2026-07-04). A scale-test boot runs `dhan_enabled=false`, so it never
/// reaches the Dhan lock — without THIS lock two hosts (Mac + AWS, or two
/// Macs) can scale the SAME Groww account simultaneously and the failure
/// masquerades as provider throttle.
///
/// Deliberately OUTSIDE the `/tickvault/<env>/groww/*` namespace: the
/// token-minter lock (`groww-shared-token-minter-2026-07-02.md`) bans
/// TickVault from WRITING any `groww/*` parameter, and this lock parameter
/// is written (Put/Delete) by the lock machinery. The name yields
/// `/tickvault/<env>/instance-lock-groww-scale` — sibling of the Dhan lock,
/// never under `groww/`.
pub const GROWW_SCALE_FLEET_LOCK_NAME: &str = "instance-lock-groww-scale";

/// Formats the SSM Parameter path for a NAMED dual-instance lock:
/// `/tickvault/<env>/<lock_name>`.
///
/// Both components are sanitised through `sanitize_ilp_symbol` so a
/// misconfigured value (`prod\n` or `prod;`) cannot inject a path break.
/// Empty / whitespace-only environments fall back to a literal
/// `"unknown"` so the path remains valid and the operator sees the issue
/// in the boot log.
#[must_use]
pub fn compute_named_lock_path(env: &str, lock_name: &str) -> String {
    // Defense-in-depth (2026-07-04 adversarial review LOW): the generic
    // `sanitize_ilp_symbol` strips control chars / ',' / '=' but NOT '/'
    // or '.', so a garbage env ("prod/groww") or lock name could inject a
    // path break and escape the `/tickvault/<env>/` namespace — including
    // into the token-minter-banned `groww/*` subtree. Production callers
    // already validate (resolve_environment allows [a-zA-Z0-9-] only;
    // lock names are &'static consts), but both fns are `pub`, so strip
    // here as well. No-op for every legitimate input; the Dhan wrapper
    // delegates here, so Dhan vs named paths stay identical by
    // construction (pinned by test_named_lock_path_dhan_name_matches_legacy_path).
    let sanitised: String = sanitize_ilp_symbol(env)
        .chars()
        .filter(|c| *c != '/' && *c != '.')
        .collect();
    let trimmed = sanitised.trim();
    let effective = if trimmed.is_empty() {
        "unknown"
    } else {
        trimmed
    };
    let name_sanitised: String = sanitize_ilp_symbol(lock_name)
        .chars()
        .filter(|c| *c != '/' && *c != '.')
        .collect();
    let name = name_sanitised.trim();
    format!("{INSTANCE_LOCK_SSM_PATH_PREFIX}/{effective}/{name}")
}

/// Formats the canonical SSM Parameter path for the Dhan dual-instance lock.
///
/// Delegates to [`compute_named_lock_path`] with the unchanged
/// [`DHAN_INSTANCE_LOCK_NAME`] — output is byte-identical to the pre-knob
/// implementation (pinned by `test_named_lock_path_dhan_name_matches_legacy_path`).
#[must_use]
pub fn compute_instance_lock_path(env: &str) -> String {
    compute_named_lock_path(env, DHAN_INSTANCE_LOCK_NAME)
}

/// Composes the `host_id` written into the lock value.
///
/// Components (joined with `:`):
///
///   * `pid` — the process ID, gives same-host uniqueness across rapid
///             restarts (the OS recycles PIDs so this is not unique
///             forever, but it's unique within a TTL window).
///   * `boot_random` — a 64-bit random value the caller draws ONCE at
///             boot. Guarantees uniqueness across hosts even if two
///             boxes happen to share a PID.
///   * `aws_instance_id` (optional) — the EC2 instance metadata tag.
///             When present, makes the audit trail human-readable
///             ("which i-0123abc was holding the lock?").
///
/// Returns a string passed through `sanitize_ilp_symbol` so it is
/// directly usable as a QuestDB SYMBOL column without further escaping.
#[must_use]
pub fn generate_host_id(pid: u32, boot_random: u64, aws_instance_id: Option<&str>) -> String {
    let raw = match aws_instance_id {
        Some(aws) if !aws.trim().is_empty() => {
            format!("{}:{pid}:{boot_random:016x}", aws.trim())
        }
        _ => format!("local:{pid}:{boot_random:016x}"),
    };
    sanitize_ilp_symbol(&raw).into_owned()
}

/// JSON shape stored in the SSM Parameter value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockValue {
    pub host_id: String,
    pub started_at_unix: u64,
    pub last_heartbeat_unix: u64,
}

impl LockValue {
    #[must_use]
    pub fn new(host_id: &str, now_unix: u64) -> Self {
        Self {
            host_id: host_id.to_string(),
            started_at_unix: now_unix,
            last_heartbeat_unix: now_unix,
        }
    }

    /// Pure function: is this lock value stale relative to `now_unix`
    /// using the given TTL? Used by acquire-takeover decisions.
    #[must_use]
    pub fn is_stale(&self, now_unix: u64, ttl_secs: u64) -> bool {
        now_unix.saturating_sub(self.last_heartbeat_unix) > ttl_secs
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("serialise LockValue to JSON")
    }

    pub fn from_json(raw: &str) -> Result<Self> {
        serde_json::from_str(raw).context("parse LockValue JSON")
    }
}

/// Outcome of a `try_acquire_instance_lock` call.
///
/// Distinct from `Result<bool, _>` so the call site can pattern-match on
/// the two outcomes without losing the live `host_id` of the other
/// instance — critical operator diagnostic on the boot-halt path ("which
/// box is already running?").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireOutcome {
    /// The lock was acquired by this process; the caller now owns the
    /// SSM Parameter and must release it on shutdown.
    Acquired,
    /// The lock was already held by a fresh peer; `holder` is the JSON
    /// `host_id` field currently stored in SSM.
    AlreadyHeld { holder: String },
}

/// Returns current Unix seconds since epoch. Wall-clock — the lock TTL
/// tolerates a few seconds of skew between hosts so monotonic clocks
/// would be wrong here.
fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Attempts to acquire the dual-instance lock for the given env.
///
/// On `AlreadyHeld` the caller MUST refuse to start. The `holder` string
/// flows into the `RESILIENCE-01` Telegram payload so the operator can
/// identify which instance is winning the race.
// TEST-EXEMPT: end-to-end behaviour requires a real AWS SSM endpoint.
// Pure-logic invariants (path format, host_id format, JSON ser/de,
// staleness) are covered by the unit tests below.
pub async fn try_acquire_instance_lock(
    ssm: &SsmClient,
    env: &str,
    host_id: &str,
) -> Result<AcquireOutcome> {
    try_acquire_lock_at_path(ssm, &compute_instance_lock_path(env), host_id).await
}

/// Shared acquire body — the SSM `PutParameter(overwrite=false)` atomic
/// claim + stale-takeover + fail-closed evaluation, against an explicit
/// parameter path. Private so the public surface stays the two thin
/// wrappers above.
async fn try_acquire_lock_at_path(
    ssm: &SsmClient,
    path: &str,
    host_id: &str,
) -> Result<AcquireOutcome> {
    let now = now_unix_secs();
    let value = LockValue::new(host_id, now);
    let payload = value.to_json()?;

    // Step 1 — atomic claim via overwrite=false. SSM rejects if the
    // parameter already exists. This is the SSM equivalent of Redis
    // SET NX.
    match put_parameter(ssm, path, &payload, false).await {
        Ok(()) => {
            info!(
                target: "tickvault::instance_lock",
                path = %path,
                host_id = %host_id,
                ttl_secs = INSTANCE_LOCK_TTL_SECS,
                "RESILIENCE-01 lock acquired (atomic claim)"
            );
            return Ok(AcquireOutcome::Acquired);
        }
        Err(err) if is_already_exists(&err) => {
            // Fall through to the read-and-evaluate path below.
        }
        Err(err) => {
            return Err(err.context(format!(
                "PutParameter(overwrite=false) failed for instance lock path={path}"
            )));
        }
    }

    // Step 2 — read the existing holder.
    let raw = match get_parameter(ssm, path).await {
        Ok(Some(value)) => value,
        Ok(None) => {
            // Rare race: the previous holder's parameter was deleted
            // between our failed Put and the Get. Refuse boot anyway —
            // re-running boot will hit the clean-slate path.
            return Ok(AcquireOutcome::AlreadyHeld {
                holder: String::new(),
            });
        }
        Err(err) => {
            warn!(
                target: "tickvault::instance_lock",
                path = %path,
                error = %err,
                "could not read existing lock holder after Put rejection \
                 (best-effort diagnostic — refusing boot)"
            );
            return Ok(AcquireOutcome::AlreadyHeld {
                holder: String::new(),
            });
        }
    };

    // Step 3 — evaluate freshness. If stale, take over via overwrite=true.
    match LockValue::from_json(&raw) {
        Ok(existing) if existing.is_stale(now, INSTANCE_LOCK_TTL_SECS) => {
            warn!(
                target: "tickvault::instance_lock",
                path = %path,
                prior_holder = %existing.host_id,
                prior_heartbeat_unix = existing.last_heartbeat_unix,
                age_secs = now.saturating_sub(existing.last_heartbeat_unix),
                "prior lock holder is stale — taking over via overwrite"
            );
            put_parameter(ssm, path, &payload, true)
                .await
                .with_context(|| {
                    format!("PutParameter(overwrite=true) takeover failed for path={path}")
                })?;
            info!(
                target: "tickvault::instance_lock",
                path = %path,
                host_id = %host_id,
                "RESILIENCE-01 lock acquired (stale takeover)"
            );
            Ok(AcquireOutcome::Acquired)
        }
        Ok(existing) => Ok(AcquireOutcome::AlreadyHeld {
            holder: existing.host_id,
        }),
        Err(parse_err) => {
            // Corrupted JSON in the SSM parameter — refuse boot. An
            // operator must clean it up (`aws ssm delete-parameter`) so
            // we never silently auto-recover from an unknown state.
            warn!(
                target: "tickvault::instance_lock",
                path = %path,
                error = %parse_err,
                raw_value = %raw,
                "instance-lock SSM value is not valid JSON — refusing boot"
            );
            Ok(AcquireOutcome::AlreadyHeld {
                holder: format!("(corrupt-json: {raw})"),
            })
        }
    }
}

/// Renews the instance lock by re-writing the SSM Parameter with a
/// refreshed `last_heartbeat_unix`.
///
/// Implements an ownership-checked overwrite: the Get-then-Put dance is
/// NOT atomic, but the worst case is that a renewal we own is briefly
/// reset to the same `host_id` — never that a different instance's lock
/// gets stomped. The 90 s TTL + 30 s renewal interval gives 3 attempts
/// before staleness, so a single transient SSM error does not lose the
/// lock.
///
/// Returns `false` if the lock no longer belongs to this process
/// (someone else won a race or the lock was deleted) — the caller MUST
/// halt the process on this signal because the dual-instance invariant
/// has broken.
// TEST-EXEMPT: end-to-end behaviour requires a real AWS SSM endpoint.
pub async fn renew_instance_lock(ssm: &SsmClient, env: &str, host_id: &str) -> Result<bool> {
    renew_lock_at_path(ssm, &compute_instance_lock_path(env), host_id).await
}

/// Shared ownership-checked renewal body against an explicit parameter path.
async fn renew_lock_at_path(ssm: &SsmClient, path: &str, host_id: &str) -> Result<bool> {
    let raw = get_parameter(ssm, path).await.with_context(|| {
        format!("GetParameter failed for instance lock path={path} during renewal")
    })?;
    let Some(raw_value) = raw else {
        // Lock vanished mid-session. Treat as lost ownership.
        return Ok(false);
    };
    let existing = match LockValue::from_json(&raw_value) {
        Ok(v) => v,
        Err(_) => return Ok(false),
    };
    if existing.host_id != host_id {
        return Ok(false);
    }
    let now = now_unix_secs();
    let refreshed = LockValue {
        host_id: existing.host_id,
        started_at_unix: existing.started_at_unix,
        last_heartbeat_unix: now,
    };
    let payload = refreshed.to_json()?;
    put_parameter(ssm, path, &payload, true)
        .await
        .with_context(|| {
            format!("PutParameter(overwrite=true) failed for path={path} during renewal")
        })?;
    Ok(true)
}

/// Releases the instance lock IFF this process still owns it.
///
/// Ownership-checked DeleteParameter: a foreign instance's lock will
/// never be stomped. Logs at WARN (not ERROR) if the lock is foreign or
/// already gone, because by the time graceful shutdown runs the lock may
/// have expired naturally — that's not an alertable condition.
// TEST-EXEMPT: end-to-end behaviour requires a real AWS SSM endpoint.
pub async fn release_instance_lock(ssm: &SsmClient, env: &str, host_id: &str) -> Result<()> {
    release_lock_at_path(ssm, &compute_instance_lock_path(env), host_id).await
}

async fn release_lock_at_path(ssm: &SsmClient, path: &str, host_id: &str) -> Result<()> {
    let raw = get_parameter(ssm, path).await.with_context(|| {
        format!("GetParameter failed for instance lock path={path} during release")
    })?;
    let Some(raw_value) = raw else {
        warn!(
            target: "tickvault::instance_lock",
            path = %path,
            "instance lock already gone at release time"
        );
        return Ok(());
    };
    let existing = match LockValue::from_json(&raw_value) {
        Ok(v) => v,
        Err(err) => {
            warn!(
                target: "tickvault::instance_lock",
                path = %path,
                error = %err,
                "instance-lock SSM value is not valid JSON at release time; leaving it alone"
            );
            return Ok(());
        }
    };
    if existing.host_id != host_id {
        warn!(
            target: "tickvault::instance_lock",
            path = %path,
            ours = %host_id,
            theirs = %existing.host_id,
            "instance lock held by another process at release time \
             (heartbeat likely raced TTL expiry); leaving foreign lock intact"
        );
        return Ok(());
    }
    ssm.delete_parameter()
        .name(path)
        .send()
        .await
        .map_err(|err| anyhow!("DeleteParameter failed for path={path}: {err}"))?;
    info!(
        target: "tickvault::instance_lock",
        path = %path,
        "instance lock released cleanly"
    );
    Ok(())
}

/// Overwrites the instance lock UNCONDITIONALLY — the operator escape
/// hatch behind the `--force-instance-takeover` CLI flag (dual-instance
/// lock hardening, operator "go" 2026-07-04).
///
/// The 90 s TTL already auto-clears a crashed holder via the stale-
/// takeover path in [`try_acquire_instance_lock`]; this function exists
/// ONLY for the wedged-lock case that TTL cannot clear (e.g. corrupt
/// JSON in the SSM parameter, or a holder whose heartbeat keeps
/// renewing but that the operator has decided must yield). It logs a
/// loud RESILIENCE-01-coded `error!` audit line (Telegram pages) naming
/// the displaced holder — a force takeover is NEVER silent.
///
/// # Errors
///
/// Propagates the SSM `PutParameter(overwrite=true)` failure.
// TEST-EXEMPT: end-to-end behaviour requires a real AWS SSM endpoint
// (same class as try_acquire_instance_lock). The pure-logic pieces
// (path format, JSON payload) are covered by the unit tests below;
// a smoke test pins the public symbol.
pub async fn force_takeover_instance_lock(ssm: &SsmClient, env: &str, host_id: &str) -> Result<()> {
    let path = compute_instance_lock_path(env);
    let now = now_unix_secs();
    // Best-effort read of the displaced holder for the audit line.
    let displaced = match get_parameter(ssm, &path).await {
        Ok(Some(raw)) => LockValue::from_json(&raw)
            .map(|v| v.host_id)
            .unwrap_or_else(|_| format!("(corrupt-json: {raw})")),
        Ok(None) => "(none — lock absent)".to_string(),
        Err(err) => format!("(unreadable: {err})"),
    };
    let payload = LockValue::new(host_id, now).to_json()?;
    put_parameter(ssm, &path, &payload, true)
        .await
        .with_context(|| {
            format!("PutParameter(overwrite=true) force-takeover failed for path={path}")
        })?;
    // Loud audit line — operator-initiated, but it MUST page so a
    // mistaken takeover against a live peer is never invisible.
    error!(
        target: "tickvault::instance_lock",
        code = ErrorCode::Resilience01DualInstanceDetected.code_str(),
        severity = ErrorCode::Resilience01DualInstanceDetected
            .severity()
            .as_str(),
        path = %path,
        host_id = %host_id,
        displaced_holder = %displaced,
        "RESILIENCE-01: OPERATOR FORCE TAKEOVER — instance lock overwritten via \
         --force-instance-takeover; the displaced instance (if live) will observe \
         lock loss within one 30s heartbeat and must halt its Dhan session"
    );
    Ok(())
}

/// Spawns the instance-lock heartbeat task.
///
/// The returned `JoinHandle` is held by the boot code so a clean shutdown
/// can wait for the final release to settle. The `shutdown` `Notify`
/// flows from the same source as every other tokio task. When notified,
/// the heartbeat exits its loop, performs ONE last `release_instance_lock`
/// so the next boot sees a clean slate immediately (instead of waiting
/// 90 s for the next instance to consider the slot stale), and returns.
///
/// Heartbeat loss-of-ownership policy: if `renew_instance_lock` returns
/// `Ok(false)` — meaning a foreign instance has stolen our slot — the
/// task logs `RESILIENCE-01` at ERROR level, flips `held_flag` to
/// `false` (so the RESILIENCE-03 mint tripwire in
/// `TokenManager::acquire_token` refuses any further
/// `generateAccessToken` calls — dual-instance lock hardening
/// 2026-07-04), and EXITS. The boot code that owns this `JoinHandle`
/// MUST observe the exit (e.g. via a watchdog or by treating lock-loss
/// as a HALT signal).
///
/// Heartbeat transient errors: an SSM API error during renewal is logged
/// at WARN and the loop continues. With a 30 s heartbeat interval against
/// a 90 s TTL, two consecutive transient failures still leave 30 s of
/// headroom before another instance considers the slot stale.
// TEST-EXEMPT: real SSM endpoint required for the GetParameter /
// PutParameter round trip; the renew_instance_lock /
// release_instance_lock functions the task delegates to are themselves
// TEST-EXEMPT for the same reason. The cancellation + exit path is small
// enough to verify by inspection.
pub fn spawn_instance_lock_heartbeat(
    ssm: Arc<SsmClient>,
    env: String,
    host_id: String,
    shutdown: Arc<Notify>,
    held_flag: Arc<std::sync::atomic::AtomicBool>,
) -> JoinHandle<()> {
    let interval = Duration::from_secs(INSTANCE_LOCK_HEARTBEAT_INTERVAL_SECS);
    tokio::spawn(async move {
        info!(
            target: "tickvault::instance_lock",
            env = %env,
            host_id = %host_id,
            interval_secs = INSTANCE_LOCK_HEARTBEAT_INTERVAL_SECS,
            ttl_secs = INSTANCE_LOCK_TTL_SECS,
            "instance-lock heartbeat task started (SSM backend)"
        );
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately — skip it so we don't redundantly
        // re-write the parameter that try_acquire just put.
        ticker.tick().await;
        loop {
            tokio::select! {
                () = shutdown.notified() => {
                    info!(
                        target: "tickvault::instance_lock",
                        "shutdown signalled — releasing instance lock"
                    );
                    // We are about to give the lock up — refuse any
                    // further mint from this process (fail-closed;
                    // RESILIENCE-03 tripwire).
                    held_flag.store(false, std::sync::atomic::Ordering::Release);
                    if let Err(err) = release_instance_lock(&ssm, &env, &host_id).await {
                        warn!(
                            target: "tickvault::instance_lock",
                            error = %err,
                            "instance-lock release on shutdown failed (slot will be \
                             considered stale within {}s by the next booting instance)",
                            INSTANCE_LOCK_TTL_SECS
                        );
                    }
                    return;
                }
                _ = ticker.tick() => {
                    match renew_instance_lock(&ssm, &env, &host_id).await {
                        Ok(true) => {
                            // Successful renewal — silent to avoid every-30s
                            // log chatter. SELFTEST-01 already provides the
                            // daily positive ping.
                        }
                        Ok(false) => {
                            // Flip BEFORE logging so the mint tripwire
                            // (RESILIENCE-03) is armed at the earliest
                            // possible instant.
                            held_flag.store(false, std::sync::atomic::Ordering::Release);
                            error!(
                                target: "tickvault::instance_lock",
                                code = ErrorCode::Resilience01DualInstanceDetected.code_str(),
                                severity = ErrorCode::Resilience01DualInstanceDetected
                                    .severity()
                                    .as_str(),
                                env = %env,
                                host_id = %host_id,
                                "instance lock lost — another process now holds the lock; \
                                 heartbeat task exiting; token mint is now REFUSED \
                                 (RESILIENCE-03 tripwire armed)"
                            );
                            return;
                        }
                        Err(err) => {
                            warn!(
                                target: "tickvault::instance_lock",
                                error = %err,
                                "transient instance-lock renewal failure; will retry on \
                                 next heartbeat tick"
                            );
                        }
                    }
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// SSM transport helpers (thin async wrappers — kept private so the public
// surface stays small and the error mapping is centralised).
// ---------------------------------------------------------------------------

async fn put_parameter(ssm: &SsmClient, path: &str, value: &str, overwrite: bool) -> Result<()> {
    ssm.put_parameter()
        .name(path)
        .value(value)
        .r#type(ParameterType::String)
        .overwrite(overwrite)
        .send()
        .await
        // DisplayErrorContext (fix 2026-07-04): `SdkError`'s bare Display is
        // just "service error" — the error CODE never reached the anyhow
        // message, so `is_already_exists` could NEVER match and a genuine
        // dual boot fail-closed Err'd instead of returning the typed
        // `AlreadyHeld` (the RESILIENCE-01 / GROWW-SCALE-05 page path).
        // The full-context formatter preserves "ParameterAlreadyExists"
        // in the chain. Pinned by
        // `test_try_acquire_instance_lock_already_held_by_fresh_peer`.
        .map_err(|err| anyhow!("{}", aws_sdk_ssm::error::DisplayErrorContext(&err)))?;
    Ok(())
}

async fn get_parameter(ssm: &SsmClient, path: &str) -> Result<Option<String>> {
    let result = ssm.get_parameter().name(path).send().await;
    match result {
        Ok(out) => Ok(out
            .parameter()
            .and_then(|p| p.value())
            .map(|v| v.to_string())),
        Err(err) => {
            // DisplayErrorContext (fix 2026-07-04): same as put_parameter —
            // the bare Display hid "ParameterNotFound", so an absent
            // parameter surfaced as a generic Err instead of Ok(None)
            // (release-after-expiry became a spurious failure). Pinned by
            // `test_renew_instance_lock_false_when_parameter_not_found_error`.
            let msg = format!("{}", aws_sdk_ssm::error::DisplayErrorContext(&err));
            if msg.contains("ParameterNotFound") {
                Ok(None)
            } else {
                Err(anyhow!("{msg}"))
            }
        }
    }
}

fn is_already_exists(err: &anyhow::Error) -> bool {
    let msg = format!("{err:?}");
    // SSM returns `ParameterAlreadyExists` when PutParameter with
    // overwrite=false hits an existing parameter.
    msg.contains("ParameterAlreadyExists")
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // AcquireOutcome — pure enum invariants (the async fns themselves are
    // TEST-EXEMPT because they need a real SSM endpoint, but the value
    // type the boot path pattern-matches on is unit-testable in isolation).
    // -----------------------------------------------------------------------

    #[test]
    fn test_acquire_outcome_acquired_and_already_held_are_distinct() {
        let a = AcquireOutcome::Acquired;
        let b = AcquireOutcome::AlreadyHeld {
            holder: "other".into(),
        };
        assert_ne!(a, b);
    }

    #[test]
    fn test_acquire_outcome_already_held_carries_holder() {
        let outcome = AcquireOutcome::AlreadyHeld {
            holder: "i-0123abc:42:0000000000000001".into(),
        };
        match outcome {
            AcquireOutcome::AlreadyHeld { holder } => assert!(holder.contains("i-0123abc")),
            AcquireOutcome::Acquired => panic!("expected AlreadyHeld"),
        }
    }

    #[test]
    fn test_acquire_outcome_already_held_empty_holder_is_valid() {
        let outcome = AcquireOutcome::AlreadyHeld {
            holder: String::new(),
        };
        match outcome {
            AcquireOutcome::AlreadyHeld { holder } => assert!(holder.is_empty()),
            AcquireOutcome::Acquired => panic!("expected AlreadyHeld"),
        }
    }

    #[test]
    fn test_try_acquire_instance_lock_smoke() {
        let _ = try_acquire_instance_lock;
    }

    #[test]
    fn test_renew_instance_lock_smoke() {
        let _ = renew_instance_lock;
    }

    #[test]
    fn test_release_instance_lock_smoke() {
        let _ = release_instance_lock;
    }

    #[test]
    fn test_spawn_instance_lock_heartbeat_smoke() {
        let _ = spawn_instance_lock_heartbeat;
    }

    #[test]
    fn test_force_takeover_instance_lock_smoke() {
        // Pins the operator escape-hatch symbol (--force-instance-takeover).
        // End-to-end behaviour is TEST-EXEMPT (real SSM endpoint required);
        // the payload + path logic it delegates to is covered by the
        // LockValue / compute_instance_lock_path tests in this module.
        let _ = force_takeover_instance_lock;
    }

    #[test]
    fn test_crashed_holder_stale_after_ttl_is_takeover_eligible() {
        // Dual-instance lock hardening 2026-07-04: a holder that hard-crashed
        // (heartbeat stopped, no release) MUST become takeover-eligible once
        // its last heartbeat is older than the TTL — this is the automatic
        // self-heal that makes the --force-instance-takeover flag a
        // wedged-lock-only tool, never a routine one.
        let crashed = LockValue::new("i-dead:1:0000000000000001", 1_000);
        // One heartbeat interval later: still fresh — NOT takeover-eligible.
        assert!(!crashed.is_stale(
            1_000 + INSTANCE_LOCK_HEARTBEAT_INTERVAL_SECS,
            INSTANCE_LOCK_TTL_SECS
        ));
        // Just past the TTL: stale — the next booting instance takes over
        // WITHOUT any operator flag.
        assert!(crashed.is_stale(1_000 + INSTANCE_LOCK_TTL_SECS + 1, INSTANCE_LOCK_TTL_SECS));
    }

    // -----------------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------------

    #[test]
    fn test_instance_lock_constants_pinned() {
        assert_eq!(INSTANCE_LOCK_TTL_SECS, 90);
        assert_eq!(INSTANCE_LOCK_HEARTBEAT_INTERVAL_SECS, 30);
        assert_eq!(INSTANCE_LOCK_SSM_PATH_PREFIX, "/tickvault");
    }

    #[test]
    fn test_heartbeat_is_one_third_of_ttl() {
        assert!(
            INSTANCE_LOCK_HEARTBEAT_INTERVAL_SECS * 3 <= INSTANCE_LOCK_TTL_SECS,
            "heartbeat must allow at least two retries before TTL expiry"
        );
    }

    // -----------------------------------------------------------------------
    // compute_instance_lock_path
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // compute_named_lock_path (Session-B fix 2026-07-04 — the named-lock knob)
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_named_lock_path_groww_scale() {
        assert_eq!(
            compute_named_lock_path("prod", GROWW_SCALE_FLEET_LOCK_NAME),
            "/tickvault/prod/instance-lock-groww-scale"
        );
        assert_eq!(
            compute_named_lock_path("dev", GROWW_SCALE_FLEET_LOCK_NAME),
            "/tickvault/dev/instance-lock-groww-scale"
        );
    }

    #[test]
    fn test_groww_scale_lock_name_is_outside_groww_namespace() {
        // Token-minter lock (2026-07-02): TickVault must NEVER write any
        // /tickvault/<env>/groww/* parameter. The fleet lock parameter is
        // written (Put/Delete) by the lock machinery, so its path must NOT
        // sit under the groww/ namespace.
        let path = compute_named_lock_path("prod", GROWW_SCALE_FLEET_LOCK_NAME);
        assert!(
            !path.contains("/groww/"),
            "fleet lock path must be OUTSIDE /tickvault/<env>/groww/*: {path}"
        );
        assert!(path.starts_with("/tickvault/prod/"), "path={path}");
    }

    #[test]
    fn test_named_lock_path_dhan_name_matches_legacy_path() {
        // The named-lock knob must keep the Dhan lock path BYTE-IDENTICAL
        // to the pre-knob implementation (`/tickvault/<env>/instance-lock`).
        for env in ["prod", "dev", "sandbox", "", "  ", "prod\nX"] {
            assert_eq!(
                compute_named_lock_path(env, DHAN_INSTANCE_LOCK_NAME),
                compute_instance_lock_path(env),
                "env={env:?}"
            );
        }
        assert_eq!(DHAN_INSTANCE_LOCK_NAME, "instance-lock");
        assert_eq!(GROWW_SCALE_FLEET_LOCK_NAME, "instance-lock-groww-scale");
    }

    #[test]
    fn test_compute_named_lock_path_sanitises_env() {
        let path = compute_named_lock_path("prod\nMALICIOUS", GROWW_SCALE_FLEET_LOCK_NAME);
        assert!(!path.contains('\n'), "newline must not survive: {path}");
        assert!(
            path.ends_with("/instance-lock-groww-scale"),
            "suffix must stay intact: {path}"
        );
        // Empty env falls back to "unknown" — same as the Dhan path fn.
        assert_eq!(
            compute_named_lock_path("", GROWW_SCALE_FLEET_LOCK_NAME),
            "/tickvault/unknown/instance-lock-groww-scale"
        );
    }

    #[test]
    fn test_compute_named_lock_path_strips_path_separators() {
        // 2026-07-04 adversarial-review LOW: a '/'-bearing env must NOT
        // inject a path break — nothing may escape /tickvault/<env>/ into
        // the token-minter-banned groww/* subtree.
        let path = compute_named_lock_path("prod/groww", GROWW_SCALE_FLEET_LOCK_NAME);
        assert_eq!(path, "/tickvault/prodgroww/instance-lock-groww-scale");
        assert!(!path.contains("/groww/"), "namespace escape: {path}");
        // '.' is stripped too (defense-in-depth).
        assert_eq!(
            compute_named_lock_path("pr.od", GROWW_SCALE_FLEET_LOCK_NAME),
            "/tickvault/prod/instance-lock-groww-scale"
        );
        // A '/'-bearing lock NAME cannot escape either.
        let name_path = compute_named_lock_path("prod", "groww/evil");
        assert_eq!(name_path, "/tickvault/prod/growwevil");
        // An env that becomes empty after stripping falls back to "unknown".
        assert_eq!(
            compute_named_lock_path("///", GROWW_SCALE_FLEET_LOCK_NAME),
            "/tickvault/unknown/instance-lock-groww-scale"
        );
    }

    #[test]
    fn test_compute_instance_lock_path_smoke() {
        // Smoke check that pins the public symbol
        // `compute_instance_lock_path` is callable; the named cases
        // below exercise every branch.
        assert_eq!(
            compute_instance_lock_path("prod"),
            "/tickvault/prod/instance-lock"
        );
    }

    #[test]
    fn test_lock_path_prod() {
        assert_eq!(
            compute_instance_lock_path("prod"),
            "/tickvault/prod/instance-lock"
        );
    }

    #[test]
    fn test_lock_path_dev() {
        assert_eq!(
            compute_instance_lock_path("dev"),
            "/tickvault/dev/instance-lock"
        );
    }

    #[test]
    fn test_lock_path_empty_env_falls_back_to_unknown() {
        assert_eq!(
            compute_instance_lock_path(""),
            "/tickvault/unknown/instance-lock"
        );
        assert_eq!(
            compute_instance_lock_path("   "),
            "/tickvault/unknown/instance-lock"
        );
    }

    #[test]
    fn test_lock_path_strips_surrounding_whitespace() {
        assert_eq!(
            compute_instance_lock_path("prod  "),
            "/tickvault/prod/instance-lock"
        );
        assert_eq!(
            compute_instance_lock_path("  prod"),
            "/tickvault/prod/instance-lock"
        );
    }

    #[test]
    fn test_lock_path_sanitises_newlines() {
        let path = compute_instance_lock_path("prod\nMALICIOUS");
        assert!(!path.contains('\n'), "newline must not survive: {path}");
        assert!(
            path.starts_with("/tickvault/"),
            "prefix must stay intact: {path}"
        );
        assert!(
            path.ends_with("/instance-lock"),
            "suffix must stay intact: {path}"
        );
    }

    // -----------------------------------------------------------------------
    // generate_host_id (unchanged from the Valkey-era module — kept as-is
    // for binary-compat with the audit log host_id field).
    // -----------------------------------------------------------------------

    #[test]
    fn test_host_id_with_aws_instance_id() {
        let id = generate_host_id(12345, 0xDEAD_BEEF_CAFE_F00D, Some("i-0123abc"));
        assert_eq!(id, "i-0123abc:12345:deadbeefcafef00d");
    }

    #[test]
    fn test_host_id_without_aws_uses_local_prefix() {
        let id = generate_host_id(12345, 0xDEAD_BEEF_CAFE_F00D, None);
        assert_eq!(id, "local:12345:deadbeefcafef00d");
    }

    #[test]
    fn test_host_id_empty_aws_falls_back_to_local() {
        let id = generate_host_id(99, 1, Some(""));
        assert_eq!(id, "local:99:0000000000000001");
        let id_ws = generate_host_id(99, 1, Some("   "));
        assert_eq!(id_ws, "local:99:0000000000000001");
    }

    #[test]
    fn test_host_id_random_is_zero_padded_16_hex() {
        let id = generate_host_id(1, 0x1, Some("i-test"));
        assert!(
            id.ends_with("0000000000000001"),
            "boot_random must be 16-hex zero-padded: {id}"
        );
    }

    #[test]
    fn test_host_id_strips_aws_whitespace() {
        let dirty = "i-0123abc\n";
        let id = generate_host_id(1, 1, Some(dirty));
        assert!(!id.contains('\n'), "newline must be stripped: {id}");
        assert!(id.starts_with("i-0123abc:"), "id={id}");
    }

    #[test]
    fn test_host_id_different_pids_produce_different_ids() {
        let a = generate_host_id(100, 42, Some("i-test"));
        let b = generate_host_id(101, 42, Some("i-test"));
        assert_ne!(a, b);
    }

    #[test]
    fn test_host_id_different_random_seeds_produce_different_ids() {
        let a = generate_host_id(100, 42, Some("i-test"));
        let b = generate_host_id(100, 43, Some("i-test"));
        assert_ne!(a, b);
    }

    #[test]
    fn test_host_id_is_ilp_safe() {
        let id = generate_host_id(1, 1, Some("i-test,with,commas\nand newlines"));
        assert!(!id.contains(','), "comma must be stripped: {id}");
        assert!(!id.contains('\n'), "newline must be stripped: {id}");
    }

    #[test]
    fn test_generate_host_id_smoke() {
        let id = generate_host_id(1, 1, Some("i-test"));
        assert!(id.contains("i-test"));
    }

    // -----------------------------------------------------------------------
    // LockValue (JSON ser/de + staleness — these are the new SSM-specific
    // pure-logic primitives. End-to-end SSM behaviour is TEST-EXEMPT.)
    // -----------------------------------------------------------------------

    #[test]
    fn test_lock_value_to_json_smoke() {
        // Pin the public symbol `LockValue::to_json` is callable. The
        // round-trip test below exercises the full serialisation path;
        // this exists so the pub-fn-test-guard's `test.*to_json`
        // matcher finds a hit.
        let v = LockValue::new("x", 1);
        assert!(v.to_json().is_ok());
    }

    #[test]
    fn test_lock_value_roundtrips_through_json() {
        let v = LockValue::new("i-abc:1:00000000deadbeef", 1_700_000_000);
        let json = v.to_json().expect("serialise");
        let back = LockValue::from_json(&json).expect("parse");
        assert_eq!(back, v);
    }

    #[test]
    fn test_lock_value_new_initialises_heartbeat_equal_to_started() {
        let v = LockValue::new("x", 12345);
        assert_eq!(v.started_at_unix, 12345);
        assert_eq!(v.last_heartbeat_unix, 12345);
    }

    #[test]
    fn test_lock_value_is_stale_fresh_returns_false() {
        let v = LockValue::new("x", 1000);
        // 30s after the heartbeat with a 90s TTL → fresh.
        assert!(!v.is_stale(1030, INSTANCE_LOCK_TTL_SECS));
    }

    #[test]
    fn test_lock_value_is_stale_at_exact_ttl_returns_false() {
        // is_stale uses strict greater-than, so exactly TTL secs old
        // is still fresh (operator-locked: prefer false-fresh over
        // false-stale to avoid takeover thrash).
        let v = LockValue::new("x", 1000);
        assert!(!v.is_stale(1000 + INSTANCE_LOCK_TTL_SECS, INSTANCE_LOCK_TTL_SECS));
    }

    #[test]
    fn test_lock_value_is_stale_past_ttl_returns_true() {
        let v = LockValue::new("x", 1000);
        assert!(v.is_stale(1000 + INSTANCE_LOCK_TTL_SECS + 1, INSTANCE_LOCK_TTL_SECS));
    }

    #[test]
    fn test_lock_value_is_stale_clock_skew_backwards_does_not_panic() {
        // saturating_sub handles backwards clock skew without panic.
        let v = LockValue::new("x", 1000);
        assert!(!v.is_stale(500, INSTANCE_LOCK_TTL_SECS));
    }

    #[test]
    fn test_lock_value_from_json_rejects_malformed_payload() {
        assert!(LockValue::from_json("not-json").is_err());
        assert!(LockValue::from_json("{}").is_err());
        assert!(LockValue::from_json(r#"{"host_id":"x"}"#).is_err());
    }

    #[test]
    fn test_lock_value_json_contains_expected_field_names() {
        let v = LockValue::new("i-abc", 42);
        let json = v.to_json().expect("serialise");
        // The JSON schema is operator-facing (it shows up in CloudWatch
        // logs + the SSM console). Pin the field names so a rename
        // doesn't silently break audit dashboards.
        assert!(json.contains("\"host_id\""));
        assert!(json.contains("\"started_at_unix\""));
        assert!(json.contains("\"last_heartbeat_unix\""));
    }

    // -----------------------------------------------------------------------
    // Local SSM stub — serves canned aws-json responses over loopback so the
    // acquire / renew / release / heartbeat SSM code paths execute
    // end-to-end WITHOUT a real AWS endpoint. House pattern: the
    // token_manager mock server (`start_mock_server`) + the notification
    // service's `endpoint_url("http://127.0.0.1:1")` SDK client.
    // -----------------------------------------------------------------------

    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    /// Starts a local stub that answers one queued `(status, body)`
    /// response per connection, in order, then stops accepting (later
    /// connects fail once the listener drops — deliberately, so tests can
    /// prove a code path issues NO further SSM call).
    async fn start_ssm_stub(
        responses: Vec<(u16, String)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ssm stub");
        let url = format!(
            "http://127.0.0.1:{}",
            listener.local_addr().expect("local addr").port()
        );
        let handle = tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                // Consume (part of) the request — the SDK tolerates the
                // server responding without draining the full body.
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 {status} STUB\r\nContent-Type: application/x-amz-json-1.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (url, handle)
    }

    /// Real `aws-sdk-ssm` client pointed at the stub (or at a
    /// connection-refused port for transport-error paths). Retries are
    /// DISABLED so each SSM call maps to exactly one stub connection.
    fn stub_ssm_client(endpoint: &str) -> SsmClient {
        let config = aws_sdk_ssm::Config::builder()
            .endpoint_url(endpoint)
            .region(aws_sdk_ssm::config::Region::new("ap-south-1"))
            .behavior_version_latest()
            .retry_config(aws_sdk_ssm::config::retry::RetryConfig::disabled())
            .credentials_provider(aws_sdk_ssm::config::Credentials::new(
                "fake-access-key",
                "fake-secret-key",
                None,
                None,
                "test",
            ))
            .build();
        SsmClient::from_conf(config)
    }

    fn put_ok_response() -> (u16, String) {
        (200, r#"{"Version":1}"#.to_string())
    }

    fn parameter_already_exists_response() -> (u16, String) {
        (
            400,
            r#"{"__type":"ParameterAlreadyExists","message":"The parameter already exists."}"#
                .to_string(),
        )
    }

    fn get_parameter_response(value: &str) -> (u16, String) {
        (
            200,
            serde_json::json!({
                "Parameter": {
                    "Name": "/tickvault/testenv/instance-lock-groww-scale",
                    "Type": "String",
                    "Value": value,
                    "Version": 1
                }
            })
            .to_string(),
        )
    }

    /// A 200 with NO `Parameter` field — `get_parameter` maps this to
    /// `Ok(None)` (parameter absent).
    fn get_parameter_absent_response() -> (u16, String) {
        (200, "{}".to_string())
    }

    // -----------------------------------------------------------------------
    // try_acquire_instance_lock — end-to-end against the stub (exercises the
    // shared try_acquire_lock_at_path body; rewired from the DELETED named-lock
    // wrappers, dead-code batch 2, 2026-07-18)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_acquire_instance_lock_acquired_on_clean_slate() {
        // PutParameter(overwrite=false) succeeds → atomic claim.
        let (url, _stub) = start_ssm_stub(vec![put_ok_response()]).await;
        let ssm = stub_ssm_client(&url);
        let outcome = try_acquire_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect("acquire against stub");
        assert_eq!(outcome, AcquireOutcome::Acquired);
    }

    #[tokio::test]
    async fn test_try_acquire_instance_lock_already_held_by_fresh_peer() {
        // Put rejected (ParameterAlreadyExists) → Get returns a FRESH
        // holder → AlreadyHeld carrying the peer's host_id.
        let fresh = LockValue::new("peer-host", now_unix_secs())
            .to_json()
            .expect("serialise fresh peer");
        let (url, _stub) = start_ssm_stub(vec![
            parameter_already_exists_response(),
            get_parameter_response(&fresh),
        ])
        .await;
        let ssm = stub_ssm_client(&url);
        let outcome = try_acquire_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect("acquire path");
        assert_eq!(
            outcome,
            AcquireOutcome::AlreadyHeld {
                holder: "peer-host".to_string()
            }
        );
    }

    #[tokio::test]
    async fn test_try_acquire_instance_lock_stale_takeover() {
        // Put rejected → Get returns a holder whose heartbeat is ancient
        // → stale takeover via PutParameter(overwrite=true) → Acquired.
        let stale = LockValue::new("dead-host", 1)
            .to_json()
            .expect("serialise stale holder");
        let (url, _stub) = start_ssm_stub(vec![
            parameter_already_exists_response(),
            get_parameter_response(&stale),
            put_ok_response(),
        ])
        .await;
        let ssm = stub_ssm_client(&url);
        let outcome = try_acquire_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect("stale takeover path");
        assert_eq!(outcome, AcquireOutcome::Acquired);
    }

    #[tokio::test]
    async fn test_try_acquire_instance_lock_corrupt_json_refuses_boot() {
        // Put rejected → Get returns garbage JSON → refuse boot with the
        // corrupt payload surfaced in the holder diagnostic (operator must
        // clean the parameter up manually — never silent auto-recovery).
        let (url, _stub) = start_ssm_stub(vec![
            parameter_already_exists_response(),
            get_parameter_response("not-json"),
        ])
        .await;
        let ssm = stub_ssm_client(&url);
        let outcome = try_acquire_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect("corrupt-json path");
        match outcome {
            AcquireOutcome::AlreadyHeld { holder } => {
                assert!(
                    holder.contains("corrupt-json"),
                    "holder must surface the corrupt payload: {holder}"
                );
            }
            AcquireOutcome::Acquired => panic!("corrupt JSON must NEVER acquire"),
        }
    }

    #[tokio::test]
    async fn test_try_acquire_instance_lock_vanished_after_put_rejection() {
        // Rare race: parameter deleted between the rejected Put and the
        // diagnostic Get → refuse boot with an empty holder.
        let (url, _stub) = start_ssm_stub(vec![
            parameter_already_exists_response(),
            get_parameter_absent_response(),
        ])
        .await;
        let ssm = stub_ssm_client(&url);
        let outcome = try_acquire_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect("vanished-parameter race path");
        assert_eq!(
            outcome,
            AcquireOutcome::AlreadyHeld {
                holder: String::new()
            }
        );
    }

    #[tokio::test]
    async fn test_try_acquire_instance_lock_get_error_refuses_boot() {
        // Stub serves ONLY the Put rejection then stops accepting — the
        // best-effort diagnostic Get fails and the acquire refuses boot
        // (fail-closed, empty holder).
        let (url, _stub) = start_ssm_stub(vec![parameter_already_exists_response()]).await;
        let ssm = stub_ssm_client(&url);
        let outcome = try_acquire_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect("get-error path refuses boot without Err");
        assert_eq!(
            outcome,
            AcquireOutcome::AlreadyHeld {
                holder: String::new()
            }
        );
    }

    #[tokio::test]
    async fn test_try_acquire_instance_lock_fail_closed_on_transport_error() {
        // Connection-refused endpoint: the overwrite=false Put fails with
        // a NON-already-exists error → fail-closed Err (cannot prove there
        // is no peer).
        let ssm = stub_ssm_client("http://127.0.0.1:1");
        let err = try_acquire_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect_err("transport error must fail closed");
        assert!(
            format!("{err:#}").contains("PutParameter(overwrite=false)"),
            "error context must name the failing SSM call: {err:#}"
        );
    }

    // -----------------------------------------------------------------------
    // renew_instance_lock — ownership-checked renewal against the stub
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_renew_instance_lock_true_when_owned() {
        let ours = LockValue::new("host-a", now_unix_secs())
            .to_json()
            .expect("serialise our value");
        let (url, _stub) =
            start_ssm_stub(vec![get_parameter_response(&ours), put_ok_response()]).await;
        let ssm = stub_ssm_client(&url);
        let renewed = renew_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect("renew against stub");
        assert!(renewed, "owned lock must renew to true");
    }

    #[tokio::test]
    async fn test_renew_instance_lock_false_when_lock_vanished() {
        let (url, _stub) = start_ssm_stub(vec![get_parameter_absent_response()]).await;
        let ssm = stub_ssm_client(&url);
        let renewed = renew_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect("vanished-lock renewal");
        assert!(!renewed, "vanished lock = lost ownership");
    }

    #[tokio::test]
    async fn test_renew_instance_lock_false_when_foreign_holder() {
        let foreign = LockValue::new("intruder-host", now_unix_secs())
            .to_json()
            .expect("serialise foreign value");
        let (url, _stub) = start_ssm_stub(vec![get_parameter_response(&foreign)]).await;
        let ssm = stub_ssm_client(&url);
        let renewed = renew_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect("foreign-holder renewal");
        assert!(!renewed, "foreign holder = lost ownership");
    }

    #[tokio::test]
    async fn test_renew_instance_lock_false_when_corrupt_json() {
        let (url, _stub) = start_ssm_stub(vec![get_parameter_response("not-json")]).await;
        let ssm = stub_ssm_client(&url);
        let renewed = renew_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect("corrupt-json renewal");
        assert!(!renewed, "corrupt value = treat as lost ownership");
    }

    #[tokio::test]
    async fn test_renew_instance_lock_false_when_parameter_not_found_error() {
        // SSM's typed ParameterNotFound error (as opposed to a 200 with no
        // Parameter field) must ALSO classify as Ok(None) → lost ownership,
        // not a transport Err. Pins the DisplayErrorContext classification
        // fix in `get_parameter` (2026-07-04).
        let (url, _stub) = start_ssm_stub(vec![(
            400,
            r#"{"__type":"ParameterNotFound","message":"Parameter not found."}"#.to_string(),
        )])
        .await;
        let ssm = stub_ssm_client(&url);
        let renewed = renew_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect("ParameterNotFound must be Ok(false), not Err");
        assert!(!renewed, "ParameterNotFound = lost ownership");
    }

    #[tokio::test]
    async fn test_renew_instance_lock_errors_on_transport_failure() {
        let ssm = stub_ssm_client("http://127.0.0.1:1");
        let err = renew_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect_err("transport failure must Err (WARN + retry at caller)");
        assert!(
            format!("{err:#}").contains("during renewal"),
            "error context must name the renewal stage: {err:#}"
        );
    }

    // -----------------------------------------------------------------------
    // release_instance_lock — ownership-checked delete against the stub
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_release_instance_lock_deletes_when_owned() {
        let ours = LockValue::new("host-a", now_unix_secs())
            .to_json()
            .expect("serialise our value");
        let (url, _stub) = start_ssm_stub(vec![
            get_parameter_response(&ours),
            (200, "{}".to_string()), // DeleteParameter OK
        ])
        .await;
        let ssm = stub_ssm_client(&url);
        release_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect("owned release must delete cleanly");
    }

    #[tokio::test]
    async fn test_release_instance_lock_ok_when_already_gone() {
        let (url, _stub) = start_ssm_stub(vec![get_parameter_absent_response()]).await;
        let ssm = stub_ssm_client(&url);
        release_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect("already-gone release is not an error");
    }

    #[tokio::test]
    async fn test_release_instance_lock_leaves_foreign_lock_intact() {
        // Only ONE stub response is queued: if release attempted a
        // DeleteParameter the second SSM call would fail and the fn would
        // Err — Ok() therefore PROVES no delete was issued for a foreign
        // holder.
        let foreign = LockValue::new("intruder-host", now_unix_secs())
            .to_json()
            .expect("serialise foreign value");
        let (url, _stub) = start_ssm_stub(vec![get_parameter_response(&foreign)]).await;
        let ssm = stub_ssm_client(&url);
        release_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect("foreign lock must be left intact, not an error");
    }

    #[tokio::test]
    async fn test_release_instance_lock_leaves_corrupt_value_alone() {
        let (url, _stub) = start_ssm_stub(vec![get_parameter_response("not-json")]).await;
        let ssm = stub_ssm_client(&url);
        release_instance_lock(&ssm, "testenv", "host-a")
            .await
            .expect("corrupt value is left alone, not an error");
    }

    // -----------------------------------------------------------------------
    // Legacy Dhan wrappers + force takeover — delegate to the same shared
    // bodies; exercised once each so the thin wrappers stay covered.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_legacy_dhan_wrappers_delegate_to_shared_bodies() {
        // Acquire (clean slate).
        let (url, _s1) = start_ssm_stub(vec![put_ok_response()]).await;
        let acquired = try_acquire_instance_lock(&stub_ssm_client(&url), "testenv", "host-a")
            .await
            .expect("legacy acquire");
        assert_eq!(acquired, AcquireOutcome::Acquired);

        // Renew (owned).
        let ours = LockValue::new("host-a", now_unix_secs())
            .to_json()
            .expect("serialise our value");
        let (url2, _s2) =
            start_ssm_stub(vec![get_parameter_response(&ours), put_ok_response()]).await;
        assert!(
            renew_instance_lock(&stub_ssm_client(&url2), "testenv", "host-a")
                .await
                .expect("legacy renew")
        );

        // Release (already gone — WARN + Ok).
        let (url3, _s3) = start_ssm_stub(vec![get_parameter_absent_response()]).await;
        release_instance_lock(&stub_ssm_client(&url3), "testenv", "host-a")
            .await
            .expect("legacy release");
    }

    #[tokio::test]
    async fn test_force_takeover_overwrites_and_names_displaced_holder() {
        // Displaced holder present: best-effort read for the audit line,
        // then unconditional PutParameter(overwrite=true).
        let displaced = LockValue::new("victim-host", now_unix_secs())
            .to_json()
            .expect("serialise displaced value");
        let (url, _s1) =
            start_ssm_stub(vec![get_parameter_response(&displaced), put_ok_response()]).await;
        force_takeover_instance_lock(&stub_ssm_client(&url), "testenv", "host-b")
            .await
            .expect("force takeover with displaced holder");

        // Lock absent: still overwrites (audit line says lock absent).
        let (url2, _s2) =
            start_ssm_stub(vec![get_parameter_absent_response(), put_ok_response()]).await;
        force_takeover_instance_lock(&stub_ssm_client(&url2), "testenv", "host-b")
            .await
            .expect("force takeover with absent lock");
    }

    // -----------------------------------------------------------------------
    // spawn_instance_lock_heartbeat — lifecycle against the stub
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_instance_lock_heartbeat_shutdown_releases_and_exits() {
        // Shutdown before the first 30s renewal tick: the select! takes
        // the shutdown arm (permit stored by notify_one), flips the flag,
        // performs the final ownership-checked release, and returns.
        let ours = LockValue::new("host-a", now_unix_secs())
            .to_json()
            .expect("serialise our value");
        let (url, _stub) = start_ssm_stub(vec![
            get_parameter_response(&ours),
            (200, "{}".to_string()), // DeleteParameter OK
        ])
        .await;
        let ssm = Arc::new(stub_ssm_client(&url));
        let shutdown = Arc::new(Notify::new());
        let held = Arc::new(AtomicBool::new(true));
        let handle = spawn_instance_lock_heartbeat(
            ssm,
            "testenv".to_string(),
            "host-a".to_string(),
            Arc::clone(&shutdown),
            Arc::clone(&held),
        );
        shutdown.notify_one();
        tokio::time::timeout(Duration::from_secs(30), handle)
            .await
            .expect("heartbeat must exit promptly on shutdown")
            .expect("heartbeat task must not panic");
        assert!(
            !held.load(Ordering::Acquire),
            "held_flag must flip false on shutdown"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_instance_lock_heartbeat_pages_and_exits_on_lost_ownership() {
        // Paused clock — the 30s renewal ticks auto-advance. Renewal #1
        // succeeds (Ok(true) re-arms the failure episode); renewal #2
        // observes a foreign holder → Ok(false) → flag flips false, the
        // RESILIENCE-01 loss page fires, and the task exits on its own
        // (NO shutdown signal sent).
        let ours = LockValue::new("host-a", now_unix_secs())
            .to_json()
            .expect("serialise our value");
        let foreign = LockValue::new("intruder-host", now_unix_secs())
            .to_json()
            .expect("serialise foreign value");
        let (url, _stub) = start_ssm_stub(vec![
            get_parameter_response(&ours),
            put_ok_response(),
            get_parameter_response(&foreign),
        ])
        .await;
        let ssm = Arc::new(stub_ssm_client(&url));
        let shutdown = Arc::new(Notify::new());
        let held = Arc::new(AtomicBool::new(true));
        let handle = spawn_instance_lock_heartbeat(
            ssm,
            "testenv".to_string(),
            "host-a".to_string(),
            Arc::clone(&shutdown),
            Arc::clone(&held),
        );
        tokio::time::timeout(Duration::from_secs(3600), handle)
            .await
            .expect("heartbeat must exit after losing ownership")
            .expect("heartbeat task must not panic");
        assert!(
            !held.load(Ordering::Acquire),
            "held_flag must flip false when ownership is lost"
        );
    }
}
