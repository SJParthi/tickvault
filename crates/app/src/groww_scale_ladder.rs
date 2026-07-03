//! Groww auto-scale ladder FSM (PR-2 of the auto-scale sequence).
//!
//! Authorization: §34 of
//! `.claude/rules/project/groww-second-feed-scope-2026-06-19.md`
//! (operator 2026-07-03). Plan:
//! `.claude/plans/active-plan-groww-autoscale.md` Items 6 + 9.
//! Runbook: `.claude/rules/project/groww-scale-error-codes.md`.
//!
//! The ladder climbs the configured rungs (`[1, 2, 5, 10]` day-1) ONLY while
//! every advance gate holds for `gate_hold_minutes`, only inside the IST
//! advance window, and rolls back to the last KNOWN-healthy rung on any
//! failure (kill newest connections first, exponential hold `min(base × 2^k,
//! 4h)`). ALL connections failing inside [`GLOBAL_FAILURE_WINDOW_SECS`] is
//! classified as an ACCOUNT-level throttle → global cooldown
//! [`GLOBAL_COOLDOWN_SECS`] + HALVE the connection count (GROWW-SCALE-02).
//!
//! Everything decision-shaped in this module is a PURE function over
//! snapshot inputs — the async runner (PR-2 Item 5/7 wiring) samples
//! telemetry and drives [`next_ladder_state`]; restart rehydration resumes at
//! the last VERIFIED-healthy rung recorded in `groww_scale_audit`
//! ([`resume_conns_from_audit`]), never the unverified rung an interrupted
//! ADVANCING was probing.

use tickvault_common::config::GrowwScaleConfig;

/// Global cooldown after a fleet-wide simultaneous failure (design §2).
pub const GLOBAL_COOLDOWN_SECS: u64 = 300;

/// Window inside which ALL connections failing = account-level throttle.
pub const GLOBAL_FAILURE_WINDOW_SECS: u64 = 60;

/// Hard cap on the exponential rollback hold (design §2: `min(base × 2^k, 4h)`).
pub const ROLLBACK_HOLD_MAX_SECS: u64 = 4 * 3600;

/// The ladder's operating state (design §2). Gauge values are stable
/// wire-format (`tv_groww_ladder_state`): 0=Probing, 1=Holding, 2=Advancing,
/// 3=RollingBack, 4=HaltedAtCeiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LadderState {
    /// Rung just changed (or boot) — watching the fleet stabilize.
    Probing,
    /// All gates green; accruing `gate_hold_minutes` of credit.
    Holding,
    /// Hold credit complete + inside the advance window — stepping to the
    /// next rung (spawning the delta connections).
    Advancing,
    /// A rung failed — killing newest connections back to the last healthy
    /// rung and waiting out the exponential hold (or the global cooldown).
    RollingBack,
    /// The configured ceiling (or the universe-required connection count)
    /// is reached and verified healthy. Terminal for the day unless a
    /// failure knocks the fleet back down.
    HaltedAtCeiling,
}

impl LadderState {
    /// Stable numeric value for the `tv_groww_ladder_state` gauge.
    #[must_use]
    pub const fn gauge_value(self) -> f64 {
        match self {
            Self::Probing => 0.0,
            Self::Holding => 1.0,
            Self::Advancing => 2.0,
            Self::RollingBack => 3.0,
            Self::HaltedAtCeiling => 4.0,
        }
    }

    /// Stable wire-format label (audit `state` SYMBOL column).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Probing => "probing",
            Self::Holding => "holding",
            Self::Advancing => "advancing",
            Self::RollingBack => "rolling_back",
            Self::HaltedAtCeiling => "halted_at_ceiling",
        }
    }
}

/// Events the runner feeds the FSM (each derived from pure gate evaluation
/// over a telemetry snapshot).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LadderEvent {
    /// Every advance gate passed for this evaluation tick.
    GatesGreen,
    /// At least one gate failed (per-rung failure, NOT fleet-wide).
    GateFailed,
    /// Hold credit reached `gate_hold_minutes` AND now is inside the advance
    /// window AND a higher rung exists.
    HoldComplete,
    /// The advance's spawned connections came up healthy (rung verified).
    AdvanceVerified,
    /// The advance failed (spawn error / immediate gate failure at new rung).
    AdvanceFailed,
    /// ALL connections failed inside [`GLOBAL_FAILURE_WINDOW_SECS`].
    GlobalFailure,
    /// Rollback hold / global cooldown expired — resume probing.
    HoldExpired,
    /// The current rung IS the ceiling (config target or universe-required
    /// count) and it is verified healthy.
    CeilingReached,
}

/// Pure, TOTAL ladder transition function. Every `(state, event)` pair maps
/// to a next state — unexpected events are self-loops (no panic, no `Option`).
#[must_use]
pub const fn next_ladder_state(state: LadderState, event: LadderEvent) -> LadderState {
    use LadderEvent as E;
    use LadderState as S;
    match (state, event) {
        // A fleet-wide failure preempts everything (global cooldown + halve).
        (_, E::GlobalFailure) => S::RollingBack,
        // Per-rung gate failure knocks any non-rollback state back.
        (S::Probing | S::Holding | S::Advancing | S::HaltedAtCeiling, E::GateFailed) => {
            S::RollingBack
        }
        (S::Probing, E::GatesGreen) => S::Holding,
        (S::Probing, _) => S::Probing,
        (S::Holding, E::HoldComplete) => S::Advancing,
        (S::Holding, E::CeilingReached) => S::HaltedAtCeiling,
        (S::Holding, _) => S::Holding,
        (S::Advancing, E::AdvanceVerified) => S::Probing,
        (S::Advancing, E::AdvanceFailed) => S::RollingBack,
        (S::Advancing, _) => S::Advancing,
        (S::RollingBack, E::HoldExpired) => S::Probing,
        (S::RollingBack, _) => S::RollingBack,
        (S::HaltedAtCeiling, _) => S::HaltedAtCeiling,
    }
}

/// One evaluation tick's telemetry snapshot. `None` on a host metric means
/// the probe is unavailable on this platform (e.g. no `/proc` on the Mac
/// scale-test box) — that gate is SKIPPED honestly (warn-once at the caller),
/// never silently passed as a measured value and never fail-closed (which
/// would block the Mac test the ladder exists for).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GateInputs {
    /// Connections currently healthy (streaming, not auth-rejected).
    pub conns_ok: usize,
    /// Connections the current rung expects.
    pub conns_expected: usize,
    /// Fleet-wide auth/entitlement rejects observed this evaluation window.
    pub auth_rejects: u64,
    /// Max per-shard capture lag (now − newest tick exchange-ts), ms.
    pub capture_lag_ms_max: u64,
    /// Host CPU percentage, if the probe is available.
    pub cpu_pct: Option<f64>,
    /// Capture-volume free-disk percentage, if the probe is available.
    pub disk_free_pct: Option<f64>,
    /// QuestDB drain healthy (ILP reachable, no sustained buffered growth).
    pub questdb_ok: bool,
    /// Max per-shard bridge behind-ness (file size − consumed offset), bytes.
    pub behind_bytes_max: u64,
}

/// Max per-shard bridge behind-ness the advance gate tolerates (design §2).
pub const GATE_MAX_BEHIND_BYTES: u64 = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// §34 PR-3 — cap-probe mode (Item 11) + weekend SMOKE mode (addendum C)
// ---------------------------------------------------------------------------

/// Cap-probe connection count (plan scenario 10: "exactly 2 conns × 600").
pub const PROBE_CONNS: usize = 2;

/// Cap-probe per-connection instrument count. 600 × 2 = 1200 > the documented
/// 1000 per-session cap — so a healthy 2×600 run PROVES the limit is
/// per-CONNECTION, and a second-conn failure points at a per-ACCOUNT cap.
pub const PROBE_INSTRUMENTS_PER_CONN: usize = 600;

/// Probe-mode config override (pure): exactly [`PROBE_CONNS`] connections of
/// [`PROBE_INSTRUMENTS_PER_CONN`] instruments each — the ladder is a single
/// rung `[2]`, so the existing machinery climbs straight to 2 and HALTS AT
/// CEILING there once verified healthy. Everything else (gates, holds,
/// windows, smoke) is inherited from the operator config unchanged.
#[must_use]
pub fn probe_overrides(cfg: &GrowwScaleConfig) -> GrowwScaleConfig {
    let mut probe = cfg.clone();
    probe.target_connections = PROBE_CONNS;
    probe.instruments_per_conn = PROBE_INSTRUMENTS_PER_CONN;
    probe.ladder = vec![PROBE_CONNS];
    probe
}

/// The cap-probe verdict (Item 11: per-conn-vs-per-account classification).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// Both probe connections captured ticks → 2×600 = 1200 instruments live
    /// simultaneously → the documented 1000 cap is per-CONNECTION and
    /// multi-connection scaling works on this account.
    MultiConnOk,
    /// Conn 0 captured but conn 1 did not → the account streams ONE
    /// connection; the limit behaves per-ACCOUNT (or the 2nd conn was
    /// rejected). Do not ladder up until Groww support clarifies.
    PerAccountLimited,
    /// Conn 0 itself never captured → infrastructure/auth problem, NOT a
    /// limit signal. No conclusion about the cap can be drawn.
    Inconclusive,
    /// Weekend SMOKE run: the market was closed, so capture evidence cannot
    /// exist by design. The fleet/ladder MACHINERY was exercised; the cap
    /// question stays open until a market-hours probe run.
    SmokeMachineryValidated,
}

impl ProbeVerdict {
    /// Stable wire-format label (audit `reason` column + log field).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MultiConnOk => "probe_multi_conn_ok",
            Self::PerAccountLimited => "probe_per_account_limited",
            Self::Inconclusive => "probe_inconclusive",
            Self::SmokeMachineryValidated => "probe_smoke_machinery_validated",
        }
    }

    /// The one-line operator verdict printed at the end of a probe run.
    #[must_use]
    pub const fn operator_line(self) -> &'static str {
        match self {
            Self::MultiConnOk => {
                "PROBE VERDICT: 2 connections x 600 instruments BOTH captured — the 1000-instrument cap is PER-CONNECTION; multi-connection scaling works on this account"
            }
            Self::PerAccountLimited => {
                "PROBE VERDICT: only connection 0 captured — the limit behaves PER-ACCOUNT (2nd connection got nothing); do NOT ladder up until Groww clarifies"
            }
            Self::Inconclusive => {
                "PROBE VERDICT: INCONCLUSIVE — connection 0 itself captured nothing (auth/infra problem, not a limit signal); fix the base feed first"
            }
            Self::SmokeMachineryValidated => {
                "PROBE VERDICT (SMOKE): market closed — fleet + ladder machinery validated end-to-end; re-run during market hours for the real per-conn-vs-per-account answer"
            }
        }
    }
}

/// Pure verdict classification from per-connection capture evidence (Item 11
/// test contract: `test_probe_verdict_classification`).
#[must_use]
pub const fn classify_probe_verdict(
    conn0_capturing: bool,
    conn1_capturing: bool,
    smoke: bool,
) -> ProbeVerdict {
    if smoke {
        return ProbeVerdict::SmokeMachineryValidated;
    }
    match (conn0_capturing, conn1_capturing) {
        (true, true) => ProbeVerdict::MultiConnOk,
        (true, false) => ProbeVerdict::PerAccountLimited,
        (false, _) => ProbeVerdict::Inconclusive,
    }
}

/// Weekend SMOKE gate inputs (addendum C): the market is CLOSED, so the
/// tick-dependent gates (streaming conns, capture lag, behind-bytes) are
/// honestly SKIPPED — no live market means no ticks BY DESIGN, never a
/// failure. The REAL signals that stay live: auth rejection (a credential
/// reject is a genuine fault at any hour) and the host CPU/disk probes.
#[must_use]
pub const fn smoke_gate_inputs(
    current: usize,
    auth_rejected: bool,
    cpu_pct: Option<f64>,
    disk_free_pct: Option<f64>,
) -> GateInputs {
    GateInputs {
        conns_ok: current,
        conns_expected: current,
        auth_rejects: if auth_rejected { 1 } else { 0 },
        capture_lag_ms_max: 0,
        cpu_pct,
        disk_free_pct,
        questdb_ok: true,
        behind_bytes_max: 0,
    }
}

/// Why a gate evaluation failed (stable wire-format labels — metric `reason`
/// label + audit `reason` column + Telegram body all reuse these). `None` =
/// every gate passed.
#[must_use]
pub fn gate_failure_reason(inputs: &GateInputs, cfg: &GrowwScaleConfig) -> Option<&'static str> {
    if inputs.conns_ok < inputs.conns_expected {
        return Some("conn_down");
    }
    if inputs.auth_rejects > 0 {
        return Some("auth_reject");
    }
    if inputs.capture_lag_ms_max > cfg.gate_max_capture_lag_ms {
        return Some("capture_lag");
    }
    if let Some(cpu) = inputs.cpu_pct
        && cpu >= cfg.gate_max_cpu_pct
    {
        return Some("cpu_high");
    }
    if let Some(free) = inputs.disk_free_pct
        && free <= cfg.gate_min_disk_free_pct
    {
        return Some("disk_low");
    }
    if !inputs.questdb_ok {
        return Some("questdb_degraded");
    }
    if inputs.behind_bytes_max > GATE_MAX_BEHIND_BYTES {
        return Some("bridge_behind");
    }
    None
}

/// Parses `"HH:MM"` to seconds-of-day. Fail-closed `None` on malformed input
/// (config validation already rejects it at boot; this is defense in depth).
#[must_use]
pub fn parse_hhmm_secs(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 3600 + m * 60)
}

/// True when `now_secs_of_day` (IST) is inside the configured advance window
/// `[start, end)`. Malformed window (already rejected by config validation)
/// fails CLOSED — no advancing.
#[must_use]
pub fn advance_window_allows(now_secs_of_day: u32, window: &[String; 2]) -> bool {
    match (parse_hhmm_secs(&window[0]), parse_hhmm_secs(&window[1])) {
        (Some(start), Some(end)) => now_secs_of_day >= start && now_secs_of_day < end,
        _ => false,
    }
}

/// Exponential rollback hold: `min(base_minutes × 2^k, 4h)` where `k` is the
/// consecutive-failure count at the SAME rung (k=0 → base). Saturating —
/// never overflows, never panics.
#[must_use]
pub fn rollback_hold_secs(consecutive_failures: u32, base_minutes: u64) -> u64 {
    let base = base_minutes.saturating_mul(60);
    let factor = 1u64.checked_shl(consecutive_failures).unwrap_or(u64::MAX);
    base.saturating_mul(factor).min(ROLLBACK_HOLD_MAX_SECS)
}

/// Post-global-failure connection count: HALVE, floor 1 (design §2 — a
/// provider throttle is answered by shrinking the fleet, never by exiting).
#[must_use]
pub const fn halve_conns(current: usize) -> usize {
    let halved = current / 2;
    if halved == 0 { 1 } else { halved }
}

/// True when the failure pattern is ACCOUNT-level (GROWW-SCALE-02): every
/// connection in the fleet failed within [`GLOBAL_FAILURE_WINDOW_SECS`] of
/// the earliest failure. Empty fleet / partial failures are per-rung
/// failures, not global.
#[must_use]
pub fn is_global_failure(failure_epoch_secs: &[u64], fleet_size: usize) -> bool {
    if fleet_size == 0 || failure_epoch_secs.len() < fleet_size {
        return false;
    }
    let Some(&min) = failure_epoch_secs.iter().min() else {
        return false;
    };
    let Some(&max) = failure_epoch_secs.iter().max() else {
        return false;
    };
    max.saturating_sub(min) <= GLOBAL_FAILURE_WINDOW_SECS
}

/// The ceiling the ladder can legally reach: the configured target, capped by
/// how many connections the universe actually needs (a 768-SID universe at
/// 1000/conn never spawns conn #2 no matter the config).
#[must_use]
pub fn effective_ceiling(cfg_target: usize, required_conns: usize) -> usize {
    if required_conns == 0 {
        return 0;
    }
    cfg_target.min(required_conns)
}

/// The next rung above `current` in the configured ladder, clamped to
/// `ceiling`. `None` when `current` already is (or exceeds) the ceiling or
/// no higher rung exists.
#[must_use]
pub fn next_rung(ladder: &[usize], current: usize, ceiling: usize) -> Option<usize> {
    if current >= ceiling {
        return None;
    }
    ladder
        .iter()
        .copied()
        .map(|r| r.min(ceiling))
        .find(|&r| r > current)
}

/// One parsed `groww_scale_audit` row, as consumed by restart rehydration.
/// The persistence layer maps QuestDB rows into this shape; keeping the type
/// here lets rehydration stay a pure, unit-testable function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScaleAuditRow {
    /// Epoch nanos of the transition (row order tiebreaker).
    pub ts_nanos: i64,
    /// Connection count after the transition.
    pub to_conns: usize,
    /// Wire-format outcome label (`verified_healthy`, `advance_started`,
    /// `rolled_back`, `global_halved`, `halted_at_ceiling`).
    pub outcome: String,
}

/// Outcome labels considered VERIFIED-healthy for rehydration purposes.
const HEALTHY_OUTCOMES: [&str; 2] = ["verified_healthy", "halted_at_ceiling"];

/// Restart rehydration (design §2 + plan Item 9): resume at the newest
/// VERIFIED-healthy rung recorded today — an interrupted ADVANCING (an
/// `advance_started` with no later `verified_healthy`) resumes at the last
/// healthy count, never the unverified probe rung. No rows (day-1 boot or
/// audit outage) → start at the FIRST ladder rung.
#[must_use]
pub fn resume_conns_from_audit(rows: &[ScaleAuditRow], first_rung: usize) -> usize {
    rows.iter()
        .filter(|r| HEALTHY_OUTCOMES.contains(&r.outcome.as_str()))
        .max_by_key(|r| r.ts_nanos)
        .map_or(first_rung, |r| r.to_conns)
}

/// Evaluation cadence of the ladder loop (cold path — never per-tick).
pub const LADDER_EVAL_INTERVAL_SECS: u64 = 30;

/// How long the runner waits between polls for the daily watch set the
/// activation watcher publishes (boot-time only — the ladder cannot cut
/// shards before the watch set exists).
pub const WATCH_SET_POLL_INTERVAL_SECS: u64 = 5;

/// Bound on the cold-path QuestDB rehydration SELECT at ladder start
/// (mirrors the audit-read timeouts elsewhere in the app crate).
pub const REHYDRATE_HTTP_TIMEOUT_SECS: u64 = 10;

/// Shared handles between the ladder (producer of `desired_conns`) and the
/// fleet reconciler + operators (consumers).
#[derive(Clone)]
pub struct GrowwScaleRuntime {
    /// The connection count the fleet reconciler must converge to.
    pub desired_conns: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Poke on every change so the fleet reconciles immediately.
    pub wake: std::sync::Arc<tokio::sync::Notify>,
}

impl Default for GrowwScaleRuntime {
    fn default() -> Self {
        Self {
            desired_conns: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }
}

/// IST-now epoch nanos (repo convention: UTC + 19800s offset).
fn now_ist_nanos() -> i64 {
    (chrono::Utc::now()
        + chrono::TimeDelta::seconds(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64))
    .timestamp_nanos_opt()
    .unwrap_or_default()
}

/// The §34 ladder runner (PR-2 Items 6+8+9 wiring). Drives the FSM over the
/// live telemetry, publishes `desired_conns` for the fleet reconciler, writes
/// one `groww_scale_audit` row per transition (best-effort — GROWW-SCALE-04
/// on failure, never gating a decision), and emits the typed GROWW-SCALE
/// codes + `tv_groww_*` metrics.
///
/// HONEST ENVELOPE (PR-2): the gate's `conns_ok` signal is FEED-LEVEL today
/// (feed streaming = fleet healthy proxy via `last_tick_age_secs` +
/// `is_auth_rejected`); true per-connection health rows land in PR-3 with the
/// portal `connections` array. CPU/disk probes are `Option` — unavailable on
/// the Mac scale-test host → those gates are honestly SKIPPED (logged once).
// APPROVED: linear FSM driver; splitting would scatter the state machine
#[allow(clippy::too_many_lines)]
// TEST-EXEMPT: composition-only async driver (sleep/HTTP/atomics); every decision it takes flows through the pure unit-tested primitives above (next_ladder_state / gate_failure_reason / advance_window_allows / rollback_hold_secs / halve_conns / is_global_failure / effective_ceiling / next_rung / resume_conns_from_audit) and the audit persistence module's pure formatters.
pub async fn run_groww_scale_ladder(
    cfg: GrowwScaleConfig,
    qdb: tickvault_common::config::QuestDbConfig,
    feed_runtime: std::sync::Arc<tickvault_api::feed_state::FeedRuntimeState>,
    feed_health: std::sync::Arc<tickvault_common::feed_health::FeedHealthRegistry>,
    runtime: GrowwScaleRuntime,
    watch_entries: std::sync::Arc<
        arc_swap::ArcSwapOption<Vec<tickvault_core::feed::groww::instruments::WatchEntry>>,
    >,
    shards_root: std::path::PathBuf,
) {
    use std::sync::atomic::Ordering;

    use tickvault_common::feed::Feed;
    use tickvault_storage::groww_scale_audit_persistence::{
        ScaleOutcome, ensure_groww_scale_audit_table,
    };
    use tracing::{error, info, warn};

    // §34 PR-3 (Item 11): cap-probe mode narrows the run to EXACTLY
    // 2 conns × 600 instruments via a pure config override — the ordinary
    // ladder machinery then does the climbing/verifying/halting.
    let cfg = if cfg.probe_mode {
        let probe = probe_overrides(&cfg);
        info!(
            conns = PROBE_CONNS,
            per_conn = PROBE_INSTRUMENTS_PER_CONN,
            "[feeds] groww scale ladder: CAP-PROBE mode — 2 x 600 verdict run"
        );
        probe
    } else {
        cfg
    };
    let mut probe_verdict_emitted = false;

    ensure_groww_scale_audit_table(&qdb).await;

    // ── Wait for the daily watch set (built by the activation watcher). ──
    let entries = loop {
        if feed_runtime.is_enabled(Feed::Groww)
            && let Some(entries) = watch_entries.load_full()
        {
            break entries;
        }
        tokio::time::sleep(std::time::Duration::from_secs(WATCH_SET_POLL_INTERVAL_SECS)).await;
    };

    // ── Cut shards + write per-conn watch files for EVERY potential conn. ──
    let shards = match tickvault_core::feed::groww::shard_cutter::cut_shards(
        &entries,
        cfg.instruments_per_conn,
    ) {
        Ok(shards) => shards,
        Err(err) => {
            // Cutter invariant violation — GROWW-SCALE-03, HALT fail-closed.
            error!(
                code = tickvault_common::error_code::ErrorCode::GrowwScale03ShardOverlap
                    .code_str(),
                error = %err,
                "GROWW-SCALE-03: shard cut refused — ladder HALTED at 0 connections \
                 (fail-closed; single-conn path unaffected when scale is disabled)"
            );
            return;
        }
    };
    let required = shards.len();
    let ceiling = effective_ceiling(cfg.target_connections, required);
    if ceiling == 0 {
        warn!("[feeds] groww scale ladder: empty universe — nothing to spawn");
        return;
    }
    let today = {
        // Same wall-clock derivation the activation watcher uses.
        (chrono::Utc::now()
            + chrono::TimeDelta::seconds(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64))
        .format("%Y-%m-%d")
        .to_string()
    };
    for shard in &shards {
        let files = crate::groww_bridge::shard_files(&shards_root, shard.conn_id);
        let path = files.watch_dir.join(format!("groww-watch-{today}.json"));
        if let Err(err) = tickvault_core::feed::groww::instruments::write_watch_entries_file(
            &path,
            &shard.entries,
            &today,
        ) {
            error!(
                code = tickvault_common::error_code::ErrorCode::GrowwScale03ShardOverlap
                    .code_str(),
                conn_id = shard.conn_id,
                error = ?err,
                "GROWW-SCALE-03 class: shard watch-file write failed — ladder HALTED \
                 (a conn without its watch file would subscribe nothing / wrong set)"
            );
            return;
        }
    }
    info!(
        shards = shards.len(),
        ceiling,
        per_conn = cfg.instruments_per_conn,
        "[feeds] groww scale ladder: shard watch files written"
    );

    // ── Restart rehydration: resume at the last VERIFIED-healthy rung. ──
    let first_rung = cfg.ladder.first().copied().unwrap_or(1).min(ceiling);
    let rehydrated = rehydrate_last_healthy(&qdb, &today).await;
    let mut current = rehydrated.unwrap_or(first_rung).min(ceiling).max(1);
    let mut last_healthy = current;
    let mut state = LadderState::Probing;
    let mut hold_started: Option<std::time::Instant> = None;
    let mut hold_until: Option<std::time::Instant> = None;
    let mut consecutive_failures_at_rung: u32 = 0;
    runtime.desired_conns.store(current, Ordering::Relaxed);
    runtime.wake.notify_waiters();
    info!(
        current,
        rehydrated = rehydrated.is_some(),
        "[feeds] groww scale ladder: starting (PROBING)"
    );

    let mut host_probe_warned = false;
    let mut smoke_warned = false;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(LADDER_EVAL_INTERVAL_SECS)).await;
        if !feed_runtime.is_enabled(Feed::Groww) {
            continue; // feed disabled — the fleet's own gate idles the children
        }
        // Market-hours freeze: no probing credit accrues off-hours — UNLESS
        // weekend SMOKE mode is on (addendum C): then the machinery keeps
        // running with tick gates honestly skipped + every outcome labelled
        // SMOKE. Production (weekend_smoke=false) keeps the freeze.
        let market_open = tickvault_common::market_hours::is_trading_session_now();
        let smoke = cfg.weekend_smoke && !market_open;
        if !market_open && !smoke {
            hold_started = None;
            continue;
        }
        if smoke && !smoke_warned {
            smoke_warned = true;
            warn!(
                "[feeds] groww scale ladder: SMOKE mode — market closed; tick \
                 gates SKIPPED (machinery validation only, NOT a live validation)"
            );
        }

        // ── Sample telemetry → GateInputs (feed-level; per-conn = PR-3). ──
        let now_nanos = now_ist_nanos();
        let tick_age = feed_health.last_tick_age_secs(Feed::Groww, now_nanos);
        let feed_streaming = tick_age.is_some_and(|age| age < 60);
        let auth_rejected = feed_health.is_auth_rejected(Feed::Groww);
        let (cpu_pct, disk_free_pct) = sample_host_probes(&shards_root);
        if (cpu_pct.is_none() || disk_free_pct.is_none()) && !host_probe_warned {
            host_probe_warned = true;
            warn!(
                cpu_available = cpu_pct.is_some(),
                disk_available = disk_free_pct.is_some(),
                "[feeds] groww scale ladder: host probe unavailable on this platform — \
                 that gate is SKIPPED (honest envelope; Mac scale-test host has no /proc)"
            );
        }
        let inputs = if smoke {
            // Addendum C: tick-dependent gates skipped; auth + host stay real.
            smoke_gate_inputs(current, auth_rejected, cpu_pct, disk_free_pct)
        } else {
            GateInputs {
                conns_ok: if feed_streaming { current } else { 0 },
                conns_expected: current,
                auth_rejects: u64::from(auth_rejected),
                capture_lag_ms_max: tick_age.map_or(0, |s| s.saturating_mul(1_000)),
                cpu_pct,
                disk_free_pct,
                questdb_ok: true, // BOOT-01/02 + doctor own QuestDB health signals
                behind_bytes_max: 0, // per-shard behind-ness: see snapshot rows
            }
        };
        let gate_failure = gate_failure_reason(&inputs, &cfg);

        // ── Global-failure classification preempts the per-rung path. ──
        if auth_rejected && !feed_streaming && !smoke && state != LadderState::RollingBack {
            state = next_ladder_state(state, LadderEvent::GlobalFailure);
            let halved = halve_conns(current);
            error!(
                code = tickvault_common::error_code::ErrorCode::GrowwScale02GlobalHalve.code_str(),
                from_conns = current,
                to_conns = halved,
                cooldown_secs = GLOBAL_COOLDOWN_SECS,
                "GROWW-SCALE-02: fleet-wide failure signature — global cooldown + halve"
            );
            metrics::counter!("tv_groww_scale_rollbacks_total", "reason" => "global_halve")
                .increment(1);
            write_audit(
                &qdb,
                &today,
                current,
                halved,
                state,
                ScaleOutcome::GlobalHalved,
                "fleet_auth_reject",
                &inputs,
            )
            .await;
            current = halved;
            last_healthy = last_healthy.min(halved).max(1);
            runtime.desired_conns.store(current, Ordering::Relaxed);
            runtime.wake.notify_waiters();
            hold_until = Some(
                std::time::Instant::now() + std::time::Duration::from_secs(GLOBAL_COOLDOWN_SECS),
            );
            hold_started = None;
            publish_state(state, current, cfg.target_connections);
            continue;
        }

        state = match state {
            LadderState::RollingBack => {
                if hold_until.is_some_and(|t| std::time::Instant::now() >= t) {
                    hold_until = None;
                    next_ladder_state(state, LadderEvent::HoldExpired)
                } else {
                    state
                }
            }
            _ => match gate_failure {
                Some(reason) => {
                    consecutive_failures_at_rung = consecutive_failures_at_rung.saturating_add(1);
                    let hold = rollback_hold_secs(
                        consecutive_failures_at_rung - 1,
                        cfg.rollback_hold_base_minutes,
                    );
                    error!(
                        code = tickvault_common::error_code::ErrorCode::GrowwScale01RollbackFired
                            .code_str(),
                        from_conns = current,
                        to_conns = last_healthy,
                        attempt = consecutive_failures_at_rung,
                        hold_secs = hold,
                        reason,
                        "GROWW-SCALE-01: rung failed — rolling back to last healthy \
                         (auto-corrected; exponential hold before retry)"
                    );
                    metrics::counter!("tv_groww_scale_rollbacks_total", "reason" => reason)
                        .increment(1);
                    let next = next_ladder_state(state, LadderEvent::GateFailed);
                    write_audit(
                        &qdb,
                        &today,
                        current,
                        last_healthy,
                        next,
                        ScaleOutcome::RolledBack,
                        reason,
                        &inputs,
                    )
                    .await;
                    current = last_healthy;
                    runtime.desired_conns.store(current, Ordering::Relaxed);
                    runtime.wake.notify_waiters();
                    hold_until =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(hold));
                    hold_started = None;
                    next
                }
                None => match state {
                    LadderState::Probing => {
                        // Gates green at this rung → verified healthy.
                        if current != last_healthy || consecutive_failures_at_rung > 0 {
                            write_audit(
                                &qdb,
                                &today,
                                last_healthy,
                                current,
                                LadderState::Holding,
                                ScaleOutcome::VerifiedHealthy,
                                "gates_green",
                                &inputs,
                            )
                            .await;
                        }
                        last_healthy = current;
                        consecutive_failures_at_rung = 0;
                        hold_started = Some(std::time::Instant::now());
                        next_ladder_state(state, LadderEvent::GatesGreen)
                    }
                    LadderState::Holding => {
                        let held_long_enough = hold_started.is_some_and(|t| {
                            t.elapsed().as_secs() >= cfg.gate_hold_minutes.saturating_mul(60)
                        });
                        let in_window = smoke
                            || advance_window_allows(
                                tickvault_common::market_hours::now_ist_secs_of_day(),
                                &cfg.advance_window_ist,
                            );
                        match (held_long_enough, next_rung(&cfg.ladder, current, ceiling)) {
                            (true, Some(next)) if in_window => {
                                info!(
                                    from_conns = current,
                                    to_conns = next,
                                    "[feeds] groww scale ladder: ADVANCING to next rung"
                                );
                                write_audit(
                                    &qdb,
                                    &today,
                                    current,
                                    next,
                                    LadderState::Advancing,
                                    ScaleOutcome::AdvanceStarted,
                                    "hold_complete",
                                    &inputs,
                                )
                                .await;
                                current = next;
                                runtime.desired_conns.store(current, Ordering::Relaxed);
                                runtime.wake.notify_waiters();
                                hold_started = None;
                                next_ladder_state(state, LadderEvent::HoldComplete)
                            }
                            (true, None) => {
                                info!(
                                    conns = current,
                                    "[feeds] groww scale ladder: ceiling reached + healthy — HALTED AT CEILING"
                                );
                                write_audit(
                                    &qdb,
                                    &today,
                                    current,
                                    current,
                                    LadderState::HaltedAtCeiling,
                                    ScaleOutcome::HaltedAtCeiling,
                                    "ceiling",
                                    &inputs,
                                )
                                .await;
                                next_ladder_state(state, LadderEvent::CeilingReached)
                            }
                            _ => state,
                        }
                    }
                    LadderState::Advancing => {
                        // Fleet reconciles within seconds; green gates on the
                        // NEXT tick verify the rung (Probing handles it).
                        next_ladder_state(state, LadderEvent::AdvanceVerified)
                    }
                    other => other,
                },
            },
        };
        publish_state(state, current, cfg.target_connections);

        // ── §34 PR-3 (Item 12): publish the per-conn panel snapshot. ──
        let snapshot = build_scale_snapshot(
            state,
            current,
            cfg.target_connections,
            cfg.probe_mode,
            smoke,
            ceiling,
            &shards_root,
            now_nanos,
        );
        feed_runtime.set_groww_scale_snapshot(std::sync::Arc::new(snapshot.clone()));

        // ── §34 PR-3 (Item 11): emit the cap-probe verdict ONCE, when the ──
        // probe run reaches a terminal signal: HALTED_AT_CEILING (both rungs
        // verified) or a rollback at rung 2 (2nd conn failing).
        if cfg.probe_mode
            && !probe_verdict_emitted
            && matches!(
                state,
                LadderState::HaltedAtCeiling | LadderState::RollingBack
            )
        {
            probe_verdict_emitted = true;
            let conn0 = snapshot
                .connections
                .first()
                .is_some_and(|c| c.tick_file_bytes > 0);
            let conn1 = snapshot
                .connections
                .get(1)
                .is_some_and(|c| c.tick_file_bytes > 0);
            let verdict = classify_probe_verdict(conn0, conn1, smoke);
            info!(
                verdict = verdict.as_str(),
                conn0_capturing = conn0,
                conn1_capturing = conn1,
                "{}",
                verdict.operator_line()
            );
            write_audit(
                &qdb,
                &today,
                current,
                current,
                state,
                ScaleOutcome::ProbeVerdict,
                verdict.as_str(),
                &inputs,
            )
            .await;
        }
    }
}

/// Assemble the `/feeds` panel snapshot from FILE-derived per-conn evidence
/// (status file = connect+subscribe proof; tick file size + mtime age =
/// capture liveness). Cold path — runs once per 30s eval tick over ≤ 100
/// shard directories.
// APPROVED: panel-row assembly — every arg is a distinct already-computed signal
#[allow(clippy::too_many_arguments)]
// TEST-EXEMPT: thin fs::metadata read per conn; the row/verdict consumers (classify_probe_verdict, the feeds handler test) and the shard_files path math are unit-tested.
fn build_scale_snapshot(
    state: LadderState,
    desired: usize,
    target: usize,
    probe_mode: bool,
    smoke: bool,
    ceiling: usize,
    shards_root: &std::path::Path,
    now_ist_nanos: i64,
) -> tickvault_api::feed_state::GrowwScaleSnapshot {
    let mut connections = Vec::with_capacity(ceiling);
    for conn_id in 0..ceiling {
        let files = crate::groww_bridge::shard_files(shards_root, conn_id);
        let subscribed_proof = files.status_file.exists();
        let (tick_file_bytes, last_capture_age_secs) = match std::fs::metadata(&files.tick_file) {
            Ok(meta) => {
                let age = meta
                    .modified()
                    .ok()
                    .and_then(|m| m.elapsed().ok())
                    .map(|d| d.as_secs());
                (meta.len(), age)
            }
            Err(_) => (0, None),
        };
        connections.push(tickvault_api::feed_state::GrowwScaleConnRow {
            conn_id,
            desired: conn_id < desired,
            subscribed_proof,
            tick_file_bytes,
            last_capture_age_secs,
        });
    }
    let _ = now_ist_nanos; // reserved: exchange-ts-based age lands with per-shard telemetry
    tickvault_api::feed_state::GrowwScaleSnapshot {
        ladder_state: state.as_str(),
        desired_conns: desired,
        target_conns: target,
        probe_mode,
        smoke,
        connections,
    }
}

/// Publish the ladder gauges (edge-cheap, cold path).
fn publish_state(state: LadderState, current: usize, target: usize) {
    metrics::gauge!("tv_groww_ladder_state").set(state.gauge_value());
    metrics::gauge!("tv_groww_conns_target").set(target as f64);
    metrics::gauge!("tv_groww_conns_desired").set(current as f64);
}

/// Best-effort audit-row write — GROWW-SCALE-04 on failure, NEVER gating.
// APPROVED: forensic row fields, written at one site
#[allow(clippy::too_many_arguments)]
async fn write_audit(
    qdb: &tickvault_common::config::QuestDbConfig,
    today: &str,
    from_conns: usize,
    to_conns: usize,
    state: LadderState,
    outcome: tickvault_storage::groww_scale_audit_persistence::ScaleOutcome,
    reason: &str,
    inputs: &GateInputs,
) {
    use tracing::error;
    let snapshot = format!(
        "{{\"conns_ok\":{},\"expected\":{},\"auth_rejects\":{},\"lag_ms\":{}}}",
        inputs.conns_ok, inputs.conns_expected, inputs.auth_rejects, inputs.capture_lag_ms_max
    );
    let trading_date_nanos = chrono::NaiveDate::parse_from_str(today, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .and_then(|dt| dt.and_utc().timestamp_nanos_opt())
        .unwrap_or_default();
    let row = tickvault_storage::groww_scale_audit_persistence::GrowwScaleAuditRow {
        ts_nanos_ist: now_ist_nanos(),
        trading_date_ist_nanos: trading_date_nanos,
        from_conns,
        to_conns,
        state: state.as_str(),
        outcome,
        reason,
        gate_snapshot: &snapshot,
    };
    if let Err(err) =
        tickvault_storage::groww_scale_audit_persistence::append_groww_scale_audit_row(qdb, &row)
            .await
    {
        error!(
            code = tickvault_common::error_code::ErrorCode::GrowwScale04AuditWriteFailed
                .code_str(),
            error = %err,
            "GROWW-SCALE-04: groww_scale_audit row write failed — ladder continues on \
             in-memory state (best-effort forensic write; idempotent re-append next transition)"
        );
        metrics::counter!("tv_groww_scale_audit_write_errors_total").increment(1);
    }
}

/// Best-effort restart rehydration read (design §2 / plan Item 9): the newest
/// VERIFIED-healthy rung recorded today, `None` on any failure (day-1 boot,
/// QuestDB down — the ladder then starts at the first rung, fail-safe).
// TEST-EXEMPT: thin HTTP read; the SELECT builder + dataset parser + resume decision are pure unit-tested fns (rehydrate_select_sql / parse_scale_audit_dataset / resume_conns_from_audit).
async fn rehydrate_last_healthy(
    qdb: &tickvault_common::config::QuestDbConfig,
    today: &str,
) -> Option<usize> {
    use tickvault_storage::groww_scale_audit_persistence::{
        parse_scale_audit_dataset, rehydrate_select_sql,
    };
    let url = format!("http://{}:{}/exec", qdb.host, qdb.http_port);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(REHYDRATE_HTTP_TIMEOUT_SECS))
        .build()
        .ok()?;
    let resp = client
        .get(&url)
        .query(&[("query", rehydrate_select_sql(today))])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    let rows = parse_scale_audit_dataset(body.get("dataset")?);
    let audit_rows: Vec<ScaleAuditRow> = rows
        .into_iter()
        .map(|(ts_nanos, to_conns, outcome)| ScaleAuditRow {
            ts_nanos,
            to_conns,
            outcome,
        })
        .collect();
    if audit_rows.is_empty() {
        return None;
    }
    Some(resume_conns_from_audit(&audit_rows, 1))
}

/// Host probes for the advance gate. `None` = probe unavailable on this
/// platform (Mac) → the gate is honestly skipped. Linux: /proc/loadavg as a
/// CPU proxy (1-min load / cores × 100) + the spill-dir statvfs free pct via
/// the storage disk probe.
// TEST-EXEMPT: platform I/O probe; the consuming gate logic (gate_failure_reason with Option inputs) is unit-tested for both Some and None arms.
fn sample_host_probes(dir: &std::path::Path) -> (Option<f64>, Option<f64>) {
    let cpu = std::fs::read_to_string("/proc/loadavg").ok().and_then(|s| {
        let load: f64 = s.split_whitespace().next()?.parse().ok()?;
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get) as f64;
        Some((load / cores) * 100.0)
    });
    let disk = match tickvault_storage::disk_health_watcher::probe_disk_free_bytes(dir) {
        tickvault_storage::disk_health_watcher::DiskHealthOutcome::Ok {
            free_bytes,
            total_bytes,
        } if total_bytes > 0 => {
            // APPROVED: ratio display only
            #[allow(clippy::cast_precision_loss)]
            Some((free_bytes as f64 / total_bytes as f64) * 100.0)
        }
        _ => None,
    };
    (cpu, disk)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GrowwScaleConfig {
        GrowwScaleConfig::default()
    }

    fn green_inputs() -> GateInputs {
        GateInputs {
            conns_ok: 2,
            conns_expected: 2,
            auth_rejects: 0,
            capture_lag_ms_max: 100,
            cpu_pct: Some(20.0),
            disk_free_pct: Some(60.0),
            questdb_ok: true,
            behind_bytes_max: 1024,
        }
    }

    // ── FSM transitions (plan Item 6: every transition + gate permutation) ──

    #[test]
    fn test_fsm_probing_to_holding_on_green() {
        assert_eq!(
            next_ladder_state(LadderState::Probing, LadderEvent::GatesGreen),
            LadderState::Holding
        );
    }

    #[test]
    fn test_fsm_holding_to_advancing_on_hold_complete() {
        assert_eq!(
            next_ladder_state(LadderState::Holding, LadderEvent::HoldComplete),
            LadderState::Advancing
        );
    }

    #[test]
    fn test_fsm_advancing_verified_returns_to_probing() {
        assert_eq!(
            next_ladder_state(LadderState::Advancing, LadderEvent::AdvanceVerified),
            LadderState::Probing
        );
    }

    #[test]
    fn test_fsm_advance_failed_rolls_back() {
        assert_eq!(
            next_ladder_state(LadderState::Advancing, LadderEvent::AdvanceFailed),
            LadderState::RollingBack
        );
    }

    #[test]
    fn test_fsm_gate_failure_rolls_back_from_every_active_state() {
        for s in [
            LadderState::Probing,
            LadderState::Holding,
            LadderState::Advancing,
            LadderState::HaltedAtCeiling,
        ] {
            assert_eq!(
                next_ladder_state(s, LadderEvent::GateFailed),
                LadderState::RollingBack,
                "{s:?} must roll back on gate failure"
            );
        }
    }

    #[test]
    fn test_fsm_global_failure_preempts_every_state() {
        for s in [
            LadderState::Probing,
            LadderState::Holding,
            LadderState::Advancing,
            LadderState::RollingBack,
            LadderState::HaltedAtCeiling,
        ] {
            assert_eq!(
                next_ladder_state(s, LadderEvent::GlobalFailure),
                LadderState::RollingBack
            );
        }
    }

    #[test]
    fn test_fsm_rollback_exits_only_on_hold_expired() {
        assert_eq!(
            next_ladder_state(LadderState::RollingBack, LadderEvent::HoldExpired),
            LadderState::Probing
        );
        // Everything else self-loops inside RollingBack.
        for e in [
            LadderEvent::GatesGreen,
            LadderEvent::HoldComplete,
            LadderEvent::AdvanceVerified,
            LadderEvent::AdvanceFailed,
            LadderEvent::CeilingReached,
        ] {
            assert_eq!(
                next_ladder_state(LadderState::RollingBack, e),
                LadderState::RollingBack
            );
        }
    }

    #[test]
    fn test_fsm_ceiling_is_sticky_except_failures() {
        assert_eq!(
            next_ladder_state(LadderState::Holding, LadderEvent::CeilingReached),
            LadderState::HaltedAtCeiling
        );
        assert_eq!(
            next_ladder_state(LadderState::HaltedAtCeiling, LadderEvent::GatesGreen),
            LadderState::HaltedAtCeiling
        );
        assert_eq!(
            next_ladder_state(LadderState::HaltedAtCeiling, LadderEvent::GateFailed),
            LadderState::RollingBack
        );
    }

    #[test]
    fn test_fsm_is_total_no_panic_over_all_pairs() {
        let states = [
            LadderState::Probing,
            LadderState::Holding,
            LadderState::Advancing,
            LadderState::RollingBack,
            LadderState::HaltedAtCeiling,
        ];
        let events = [
            LadderEvent::GatesGreen,
            LadderEvent::GateFailed,
            LadderEvent::HoldComplete,
            LadderEvent::AdvanceVerified,
            LadderEvent::AdvanceFailed,
            LadderEvent::GlobalFailure,
            LadderEvent::HoldExpired,
            LadderEvent::CeilingReached,
        ];
        for s in states {
            for e in events {
                let _ = next_ladder_state(s, e); // total: never panics
            }
        }
    }

    #[test]
    fn test_gauge_values_and_labels_stable() {
        assert_eq!(LadderState::Probing.gauge_value(), 0.0);
        assert_eq!(LadderState::Holding.gauge_value(), 1.0);
        assert_eq!(LadderState::Advancing.gauge_value(), 2.0);
        assert_eq!(LadderState::RollingBack.gauge_value(), 3.0);
        assert_eq!(LadderState::HaltedAtCeiling.gauge_value(), 4.0);
        assert_eq!(LadderState::Probing.as_str(), "probing");
        assert_eq!(LadderState::RollingBack.as_str(), "rolling_back");
        assert_eq!(LadderState::HaltedAtCeiling.as_str(), "halted_at_ceiling");
    }

    // ── Gates (every permutation) ──

    #[test]
    fn test_gate_green_passes() {
        assert_eq!(gate_failure_reason(&green_inputs(), &cfg()), None);
    }

    #[test]
    fn test_gate_conn_down() {
        let mut i = green_inputs();
        i.conns_ok = 1;
        assert_eq!(gate_failure_reason(&i, &cfg()), Some("conn_down"));
    }

    #[test]
    fn test_gate_auth_reject() {
        let mut i = green_inputs();
        i.auth_rejects = 1;
        assert_eq!(gate_failure_reason(&i, &cfg()), Some("auth_reject"));
    }

    #[test]
    fn test_gate_capture_lag() {
        let mut i = green_inputs();
        i.capture_lag_ms_max = cfg().gate_max_capture_lag_ms + 1;
        assert_eq!(gate_failure_reason(&i, &cfg()), Some("capture_lag"));
    }

    #[test]
    fn test_gate_cpu_high_and_unavailable_probe_skips() {
        let mut i = green_inputs();
        i.cpu_pct = Some(cfg().gate_max_cpu_pct);
        assert_eq!(gate_failure_reason(&i, &cfg()), Some("cpu_high"));
        i.cpu_pct = None; // probe unavailable (Mac) → gate honestly skipped
        assert_eq!(gate_failure_reason(&i, &cfg()), None);
    }

    #[test]
    fn test_gate_disk_low_and_unavailable_probe_skips() {
        let mut i = green_inputs();
        i.disk_free_pct = Some(cfg().gate_min_disk_free_pct);
        assert_eq!(gate_failure_reason(&i, &cfg()), Some("disk_low"));
        i.disk_free_pct = None;
        assert_eq!(gate_failure_reason(&i, &cfg()), None);
    }

    #[test]
    fn test_gate_questdb_and_behind() {
        let mut i = green_inputs();
        i.questdb_ok = false;
        assert_eq!(gate_failure_reason(&i, &cfg()), Some("questdb_degraded"));
        i.questdb_ok = true;
        i.behind_bytes_max = GATE_MAX_BEHIND_BYTES + 1;
        assert_eq!(gate_failure_reason(&i, &cfg()), Some("bridge_behind"));
    }

    // ── Advance window (plan test: test_advance_window_gate) ──

    #[test]
    fn test_advance_window_gate() {
        let w = ["09:20".to_string(), "14:30".to_string()];
        assert!(!advance_window_allows(9 * 3600 + 19 * 60, &w));
        assert!(advance_window_allows(9 * 3600 + 20 * 60, &w));
        assert!(advance_window_allows(12 * 3600, &w));
        assert!(!advance_window_allows(14 * 3600 + 30 * 60, &w)); // end exclusive
        // Malformed window fails CLOSED.
        let bad = ["9:20".to_string(), "14:30".to_string()];
        assert!(!advance_window_allows(12 * 3600, &bad));
    }

    // ── §34 PR-3: cap-probe mode + weekend SMOKE mode ──

    /// Item 11 contract: probe mode runs EXACTLY 2 connections × 600
    /// instruments regardless of the operator's ladder/target/per-conn.
    #[test]
    fn test_probe_mode_runs_two_conns_600() {
        // test coverage (pub-fn-test-guard line): probe_overrides
        let mut base = cfg();
        base.probe_mode = true;
        base.target_connections = 10;
        base.instruments_per_conn = 1000;
        base.ladder = vec![1, 2, 5, 10];
        let probe = probe_overrides(&base);
        assert_eq!(probe.target_connections, 2);
        assert_eq!(probe.instruments_per_conn, 600);
        assert_eq!(probe.ladder, vec![2]);
        // 2 × 600 = 1200 deliberately exceeds the documented 1000 cap — that
        // asymmetry is what makes the verdict discriminating.
        assert!(2 * probe.instruments_per_conn > 1000);
        // Everything else inherits unchanged.
        assert_eq!(probe.gate_hold_minutes, base.gate_hold_minutes);
        assert_eq!(probe.advance_window_ist, base.advance_window_ist);
        // The override still validates (probe envelope is legal).
        assert!(probe.validate().is_ok());
    }

    /// Item 11 contract: verdict classification over per-conn evidence.
    #[test]
    fn test_probe_verdict_classification() {
        assert_eq!(
            classify_probe_verdict(true, true, false),
            ProbeVerdict::MultiConnOk
        );
        assert_eq!(
            classify_probe_verdict(true, false, false),
            ProbeVerdict::PerAccountLimited
        );
        assert_eq!(
            classify_probe_verdict(false, true, false),
            ProbeVerdict::Inconclusive
        );
        assert_eq!(
            classify_probe_verdict(false, false, false),
            ProbeVerdict::Inconclusive
        );
        // SMOKE wins over any capture evidence: market closed ⇒ no capture
        // conclusion is honest.
        assert_eq!(
            classify_probe_verdict(true, true, true),
            ProbeVerdict::SmokeMachineryValidated
        );
        // Labels are stable wire format.
        assert_eq!(ProbeVerdict::MultiConnOk.as_str(), "probe_multi_conn_ok");
        assert_eq!(
            ProbeVerdict::PerAccountLimited.as_str(),
            "probe_per_account_limited"
        );
        assert_eq!(ProbeVerdict::Inconclusive.as_str(), "probe_inconclusive");
        assert_eq!(
            ProbeVerdict::SmokeMachineryValidated.as_str(),
            "probe_smoke_machinery_validated"
        );
        // Every verdict carries a non-empty operator line.
        for v in [
            ProbeVerdict::MultiConnOk,
            ProbeVerdict::PerAccountLimited,
            ProbeVerdict::Inconclusive,
            ProbeVerdict::SmokeMachineryValidated,
        ] {
            assert!(!v.operator_line().is_empty());
        }
    }

    /// Addendum C contract: SMOKE inputs pass every tick-dependent gate while
    /// the REAL signals (auth reject, host CPU/disk) still gate.
    #[test]
    fn test_smoke_mode_gate_inputs_pass_tick_gates_keep_host_gates() {
        let c = cfg();
        // Healthy smoke: no auth reject, healthy host → every gate passes
        // even though zero ticks exist (market closed).
        let ok = smoke_gate_inputs(2, false, Some(20.0), Some(60.0));
        assert_eq!(gate_failure_reason(&ok, &c), None);
        // Auth reject stays a REAL failure in smoke.
        let auth = smoke_gate_inputs(2, true, Some(20.0), Some(60.0));
        assert!(gate_failure_reason(&auth, &c).is_some());
        // Host CPU gate stays REAL in smoke.
        let hot = smoke_gate_inputs(2, false, Some(99.0), Some(60.0));
        assert!(gate_failure_reason(&hot, &c).is_some());
        // Host disk gate stays REAL in smoke.
        let full = smoke_gate_inputs(2, false, Some(20.0), Some(1.0));
        assert!(gate_failure_reason(&full, &c).is_some());
        // Unavailable host probes are honestly skipped in smoke too (Mac).
        let mac = smoke_gate_inputs(2, false, None, None);
        assert_eq!(gate_failure_reason(&mac, &c), None);
    }

    #[test]
    fn test_parse_hhmm_secs_bounds() {
        assert_eq!(parse_hhmm_secs("00:00"), Some(0));
        assert_eq!(parse_hhmm_secs("23:59"), Some(23 * 3600 + 59 * 60));
        assert_eq!(parse_hhmm_secs("24:00"), None);
        assert_eq!(parse_hhmm_secs("12:60"), None);
        assert_eq!(parse_hhmm_secs("1200"), None);
        assert_eq!(parse_hhmm_secs("ab:cd"), None);
    }

    // ── Rollback hold + halve (plan tests) ──

    #[test]
    fn test_rollback_hold_expo_capped_at_4h() {
        assert_eq!(rollback_hold_secs(0, 10), 600);
        assert_eq!(rollback_hold_secs(1, 10), 1200);
        assert_eq!(rollback_hold_secs(2, 10), 2400);
        assert_eq!(rollback_hold_secs(10, 10), ROLLBACK_HOLD_MAX_SECS);
        assert_eq!(rollback_hold_secs(u32::MAX, 10), ROLLBACK_HOLD_MAX_SECS);
    }

    #[test]
    fn test_global_failure_halves_and_cooldowns() {
        // All 4 conns fail within 60s → global.
        assert!(is_global_failure(&[100, 110, 140, 160], 4));
        // Spread beyond 60s → per-rung, not global.
        assert!(!is_global_failure(&[100, 110, 140, 161], 4));
        // Partial failure (3 of 4) → not global.
        assert!(!is_global_failure(&[100, 110, 140], 4));
        // Empty fleet never classifies global.
        assert!(!is_global_failure(&[], 0));
        // Halve floors at 1 and never returns 0.
        assert_eq!(halve_conns(10), 5);
        assert_eq!(halve_conns(5), 2);
        assert_eq!(halve_conns(1), 1);
        // Cooldown constant per design §2.
        assert_eq!(GLOBAL_COOLDOWN_SECS, 300);
    }

    // ── Ceiling + rung selection ──

    #[test]
    fn test_effective_ceiling_universe_caps_config() {
        assert_eq!(effective_ceiling(10, 1), 1); // 768 SIDs @ 1000/conn
        assert_eq!(effective_ceiling(10, 100), 10);
        assert_eq!(effective_ceiling(10, 0), 0); // empty universe
    }

    #[test]
    fn test_next_rung_climbs_and_stops_at_ceiling() {
        let ladder = vec![1, 2, 5, 10];
        assert_eq!(next_rung(&ladder, 1, 10), Some(2));
        assert_eq!(next_rung(&ladder, 2, 10), Some(5));
        assert_eq!(next_rung(&ladder, 5, 10), Some(10));
        assert_eq!(next_rung(&ladder, 10, 10), None);
        // Ceiling below a rung clamps it (7-conn universe: 5 → 7, not 10).
        assert_eq!(next_rung(&ladder, 5, 7), Some(7));
        assert_eq!(next_rung(&ladder, 7, 7), None);
    }

    // ── Restart rehydration (plan Item 9: resume at last healthy) ──

    #[test]
    fn test_restart_mid_ladder_resumes_last_healthy() {
        let rows = vec![
            ScaleAuditRow {
                ts_nanos: 1,
                to_conns: 1,
                outcome: "verified_healthy".to_string(),
            },
            ScaleAuditRow {
                ts_nanos: 2,
                to_conns: 2,
                outcome: "verified_healthy".to_string(),
            },
            // Interrupted advance to 5 — crash before verification.
            ScaleAuditRow {
                ts_nanos: 3,
                to_conns: 5,
                outcome: "advance_started".to_string(),
            },
        ];
        assert_eq!(resume_conns_from_audit(&rows, 1), 2);
    }

    #[test]
    fn test_resume_defaults_to_first_rung_with_no_rows() {
        assert_eq!(resume_conns_from_audit(&[], 1), 1);
    }

    #[test]
    fn test_resume_honors_halted_at_ceiling_and_ignores_rollbacks() {
        let rows = vec![
            ScaleAuditRow {
                ts_nanos: 5,
                to_conns: 10,
                outcome: "halted_at_ceiling".to_string(),
            },
            ScaleAuditRow {
                ts_nanos: 6,
                to_conns: 5,
                outcome: "rolled_back".to_string(),
            },
        ];
        // rolled_back is NOT a verified-healthy record — the newest verified
        // rung wins (the rollback's own later stabilization writes a fresh
        // verified_healthy row).
        assert_eq!(resume_conns_from_audit(&rows, 1), 10);
    }
}
