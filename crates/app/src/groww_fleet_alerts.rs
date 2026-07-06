//! Fleet-scoped Telegram alert coalescing for the Groww auto-scale fleet
//! (§34, exam-fix directive 2026-07-06).
//!
//! # Why this exists (the 2026-07-06 exam incident)
//!
//! During the fleet exam the operator received DOZENS of alternating
//! per-connection Telegram messages — every one of the ~86 fleet connections
//! independently fired its `[HIGH] Groww live feed rejected …` /
//! `the feed reported an error and is retrying` alert through the sidecar
//! supervisor path, and every shard bridge independently fired its
//! `[LOW] Groww feed connected — Subscribed …` ping, one per connection per
//! retry cycle. The core `TelegramCoalescer` cannot absorb this: it
//! deliberately bypasses `Severity::High` events (`GrowwSidecarRejected`)
//! and `DispatchPolicy::Immediate` events, so fleet-wide storms reached the
//! operator raw.
//!
//! # The contract
//!
//! - **Single-connection path (`conn_id == None`)**: byte-identical to
//!   today — every decision is [`FleetAlertDecision::Passthrough`]; no
//!   window, no state.
//! - **Fleet path (`conn_id == Some(n)`)**: reject/retry and connected
//!   transitions across ALL fleet connections aggregate into at most ONE
//!   Telegram per [`FLEET_ALERT_WINDOW_SECS`] window PER DIRECTION
//!   ([`FleetAlertKind::Reject`] vs [`FleetAlertKind::Connected`]), with a
//!   count summary ("7 of 40 connections retrying … — no reject reported
//!   from the other 33 connections").
//!
//! # Honest envelope
//!
//! The summary is edge-triggered (audit-findings Rule 4): the FIRST event of
//! a window emits with the count known AT THAT INSTANT; connections that
//! join the reject set later in the same window are counted by the NEXT
//! window's summary (the reject set persists across windows; the caller's
//! per-child latch is NOT consumed by a [`FleetAlertDecision::Suppress`], so
//! suppressed connections retry into later windows and the accumulated count
//! genuinely reaches the operator — hostile-review fix 2026-07-06). There is
//! deliberately NO delayed/debounced flush task — no timer, no new
//! background task, no new silent-death surface. Per-connection `error!`
//! logs, feed-health state, and `ws_event_audit` forensic rows are NEVER
//! coalesced — only the operator-facing Telegram fan-out is.
//!
//! The summary NEVER makes a positive health claim about connections outside
//! the reject set (audit-findings Rule 11: no false-OK; connected ≠
//! streaming per `ws-event-audit` §1). Absence from the reject set means
//! only "no reject reported" — a connection may still be booting or silently
//! starving before its own watchdog fires — and the wording says exactly
//! that. A connection is REMOVED from the reject set only on the supervisor's
//! genuine streaming/auth-OK recovery edge ([`record_fleet_recovery`]) or on
//! fleet scale-down pruning ([`FleetAlertState::set_fleet_size`]) — NEVER on
//! the socket-connected (awaiting-first-tick) edge, which proves nothing
//! about data flow.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// At most ONE fleet Telegram per direction inside this window (seconds).
/// Matches the core coalescer's `DEFAULT_WINDOW_SECS` so operator cadence
/// expectations stay uniform.
pub const FLEET_ALERT_WINDOW_SECS: u64 = 60;

/// Direction of a fleet-level operator alert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FleetAlertKind {
    /// A connection's sidecar printed an alert-class line (reject / retry).
    Reject,
    /// A connection reported connected/subscribed (the boot-connect ping).
    Connected,
}

/// What the caller should do with the operator-facing Telegram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FleetAlertDecision {
    /// Single-connection (non-fleet) path — emit exactly as today.
    Passthrough,
    /// First event of this direction's window — emit ONE summary carrying
    /// the count of connections currently affected + the fleet size.
    EmitSummary {
        /// Connections currently in the reject set (Reject direction) or
        /// connections with NO reject currently recorded (Connected
        /// direction — "no reject reported", NOT proven healthy).
        affected: usize,
        /// Fleet size as last published by the reconciler (0 until known).
        fleet_size: usize,
    },
    /// Within an already-summarized window — skip the Telegram (logs,
    /// feed-health, and audit rows still fire at the call site).
    Suppress,
}

/// Mutable fleet alert state. Cold path only (a handful of transitions per
/// minute at worst); sizes are bounded by the 100-connection config hard max.
#[derive(Debug, Default)]
pub struct FleetAlertState {
    fleet_size: usize,
    rejecting: HashSet<usize>,
    last_reject_emit_secs: Option<u64>,
    last_connected_emit_secs: Option<u64>,
}

impl FleetAlertState {
    /// Publish the current fleet size (called by the fleet reconciler each
    /// pass) AND prune reject entries for scaled-down connections. Conn ids
    /// are contiguous `0..n` by construction (the reconciler spawns with
    /// `conn_id = children.len()` and scale-down pops the NEWEST), so any
    /// id `>= n` belongs to a killed connection that can never produce a
    /// recovery edge — leaving it in the set would inflate every later
    /// summary toward a false whole-fleet-down claim (hostile-review fix
    /// 2026-07-06: GROWW-SCALE-01 rollback kills exactly the connections
    /// that just rejected).
    pub fn set_fleet_size(&mut self, n: usize) {
        self.fleet_size = n;
        self.rejecting.retain(|conn| *conn < n);
    }

    /// Number of connections currently in the reject set (for tests +
    /// observability).
    #[must_use]
    pub fn rejecting_count(&self) -> usize {
        self.rejecting.len()
    }
}

/// Pure fleet-alert decision. `conn_id == None` = the single-connection
/// path (today's semantics, untouched). For fleet connections the reject
/// set is updated, then the per-direction 60s window decides emit-vs-
/// suppress. A backwards clock step (`now_secs` < last emit) saturates to
/// a 0s delta → Suppress (the window only ever widens, never double-fires).
pub fn decide_fleet_alert(
    state: &mut FleetAlertState,
    conn_id: Option<usize>,
    kind: FleetAlertKind,
    now_secs: u64,
) -> FleetAlertDecision {
    let Some(conn) = conn_id else {
        return FleetAlertDecision::Passthrough;
    };
    match kind {
        FleetAlertKind::Reject => {
            state.rejecting.insert(conn);
        }
        FleetAlertKind::Connected => {
            // Deliberately does NOT touch the reject set (hostile-review fix
            // 2026-07-06): the bridge feeds this on the boot-connect
            // "awaiting first tick" edge — socket-connected, NOT streaming.
            // Clearing here would let a connect→silence→reject cycle (the
            // exam's server-side session starvation pattern) chronically
            // undercount every reject summary. The ONLY reject-set clear
            // edges are the supervisor's streaming/auth-OK recovery
            // ([`record_fleet_recovery`]) and scale-down pruning
            // ([`FleetAlertState::set_fleet_size`]).
        }
    }
    let last = match kind {
        FleetAlertKind::Reject => &mut state.last_reject_emit_secs,
        FleetAlertKind::Connected => &mut state.last_connected_emit_secs,
    };
    let window_open = last.is_none_or(|t| now_secs.saturating_sub(t) >= FLEET_ALERT_WINDOW_SECS);
    if !window_open {
        return FleetAlertDecision::Suppress;
    }
    *last = Some(now_secs);
    let affected = match kind {
        FleetAlertKind::Reject => state.rejecting.len(),
        // Connected direction: connections with no reject currently recorded
        // ("no reject reported" — NOT proven streaming; at least the
        // reporting connection itself, so the count is never a misleading 0).
        FleetAlertKind::Connected => state
            .fleet_size
            .saturating_sub(state.rejecting.len())
            .max(1),
    };
    FleetAlertDecision::EmitSummary {
        affected,
        // The fleet size can lag the reconciler by one pass; never report a
        // fleet smaller than the affected count (no "3 of 2" nonsense).
        fleet_size: state.fleet_size.max(affected),
    }
}

/// Recovery-edge bookkeeping WITHOUT consuming the Connected window: the
/// sidecar supervisor's streaming-recovery line clears this connection from
/// the reject set silently (it emits no Telegram of its own), leaving the
/// Connected window free for the bridge's genuine connected ping.
pub fn record_fleet_recovery(state: &mut FleetAlertState, conn_id: Option<usize>) {
    if let Some(conn) = conn_id {
        state.rejecting.remove(&conn);
    }
}

/// Render the fleet reject summary in plain operator English (Telegram
/// commandment 1: no library names, no file paths). Pure.
///
/// The suffix is deliberately NEGATIVE-evidence-only ("no reject reported"),
/// never a positive health claim ("streaming normally") — membership outside
/// the reject set proves nothing about data flow (audit-findings Rule 11;
/// hostile-review fix 2026-07-06).
#[must_use]
pub fn format_fleet_reject_summary(affected: usize, fleet_size: usize, label: &str) -> String {
    if affected >= fleet_size {
        format!("{affected} of {fleet_size} connections retrying ({label})")
    } else {
        let remainder = fleet_size.saturating_sub(affected);
        format!(
            "{affected} of {fleet_size} connections retrying ({label}) — \
             no reject reported from the other {remainder} connections"
        )
    }
}

/// Process-wide shared fleet alert state. One instance per process — the
/// fleet reconciler, every per-connection sidecar supervisor, and every
/// shard bridge feed the SAME window so coalescing is genuinely
/// fleet-scoped. O(1) atomic load after first call (same `OnceLock`
/// pattern as `shared_probe_client`).
fn global_state() -> &'static Mutex<FleetAlertState> {
    static STATE: OnceLock<Mutex<FleetAlertState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(FleetAlertState::default()))
}

/// Poisoning-safe lock (a panicked holder must not disable fleet alerting
/// forever — same `into_inner` recovery pattern the token-guard tests use).
fn lock_global() -> std::sync::MutexGuard<'static, FleetAlertState> {
    global_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Wall-clock seconds since the UNIX epoch (cold path; a clock error
/// degrades to 0 which only widens the suppression window — fail-quiet,
/// never fail-loud-storm).
fn now_wall_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Publish the live fleet size into the process-wide state (called by the
/// fleet reconciler each pass; the single-conn boot path never calls this,
/// so non-fleet behavior is untouched).
pub fn set_global_fleet_size(n: usize) {
    lock_global().set_fleet_size(n);
}

/// Fleet-scoped decision against the process-wide state at wall-clock now.
#[must_use]
pub fn global_fleet_alert_decision(
    conn_id: Option<usize>,
    kind: FleetAlertKind,
) -> FleetAlertDecision {
    if conn_id.is_none() {
        // Single-conn fast path: no lock, no state mutation.
        return FleetAlertDecision::Passthrough;
    }
    decide_fleet_alert(&mut lock_global(), conn_id, kind, now_wall_secs())
}

/// Recovery-edge bookkeeping against the process-wide state (see
/// [`record_fleet_recovery`]).
pub fn global_record_fleet_recovery(conn_id: Option<usize>) {
    if conn_id.is_some() {
        record_fleet_recovery(&mut lock_global(), conn_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // pub-fn-test-guard one-liners (each fn is exercised by the tests below):
    // tests exercise decide_fleet_alert across every arm.
    // tests exercise set_fleet_size (denominator publishing).
    // tests exercise rejecting_count (state observability).
    // tests exercise record_fleet_recovery (silent recovery bookkeeping).
    // tests exercise format_fleet_reject_summary (operator wording).
    // tests exercise set_global_fleet_size via the global wrapper test.
    // tests exercise global_fleet_alert_decision via the global wrapper test.
    // tests exercise global_record_fleet_recovery via the global wrapper test.

    /// 1 conn (non-fleet, `conn_id == None`) = passthrough — today's
    /// single-connection semantics are byte-identical.
    #[test]
    fn test_single_conn_is_passthrough_and_stateless() {
        let mut state = FleetAlertState::default();
        for kind in [FleetAlertKind::Reject, FleetAlertKind::Connected] {
            assert_eq!(
                decide_fleet_alert(&mut state, None, kind, 1_000),
                FleetAlertDecision::Passthrough
            );
        }
        // No state was touched — a later fleet event still opens a fresh window.
        assert_eq!(state.rejecting_count(), 0);
        assert_eq!(
            decide_fleet_alert(&mut state, Some(0), FleetAlertKind::Reject, 1_000),
            FleetAlertDecision::EmitSummary {
                affected: 1,
                fleet_size: 1
            }
        );
    }

    /// N conns rejecting inside the same 60s window = exactly ONE summary;
    /// the rest suppress (the exam-spam fix).
    #[test]
    fn test_n_conns_same_window_coalesce_to_one_summary() {
        let mut state = FleetAlertState::default();
        state.set_fleet_size(40);
        assert_eq!(
            decide_fleet_alert(&mut state, Some(0), FleetAlertKind::Reject, 100),
            FleetAlertDecision::EmitSummary {
                affected: 1,
                fleet_size: 40
            }
        );
        for conn in 1..7 {
            assert_eq!(
                decide_fleet_alert(
                    &mut state,
                    Some(conn),
                    FleetAlertKind::Reject,
                    100 + conn as u64
                ),
                FleetAlertDecision::Suppress,
                "conn {conn} must be suppressed inside the window"
            );
        }
        // All 7 stayed tracked for the NEXT window's count.
        assert_eq!(state.rejecting_count(), 7);
    }

    /// Window rollover: the first event ≥ 60s after the last summary emits a
    /// fresh summary carrying the ACCUMULATED reject count.
    #[test]
    fn test_window_rollover_emits_fresh_summary_with_accumulated_count() {
        let mut state = FleetAlertState::default();
        state.set_fleet_size(40);
        let _first = decide_fleet_alert(&mut state, Some(0), FleetAlertKind::Reject, 100);
        for conn in 1..7 {
            let _ = decide_fleet_alert(&mut state, Some(conn), FleetAlertKind::Reject, 101);
        }
        // 59s later — still the same window.
        assert_eq!(
            decide_fleet_alert(&mut state, Some(1), FleetAlertKind::Reject, 159),
            FleetAlertDecision::Suppress
        );
        // 60s later — new window; the summary now says "7 of 40".
        assert_eq!(
            decide_fleet_alert(&mut state, Some(1), FleetAlertKind::Reject, 160),
            FleetAlertDecision::EmitSummary {
                affected: 7,
                fleet_size: 40
            }
        );
    }

    /// Recovery edge: ONLY the supervisor's streaming/auth-OK recovery
    /// (`record_fleet_recovery`) clears the reject set. The bridge's
    /// Connected (socket-connected, awaiting-first-tick) event has its OWN
    /// window but must NOT clear the reject set — the exam's
    /// connect→silence→reject cycle would otherwise chronically undercount
    /// every reject summary (hostile-review fix 2026-07-06).
    #[test]
    fn test_recovery_edge_clears_reject_set_and_connected_window_is_independent() {
        let mut state = FleetAlertState::default();
        state.set_fleet_size(3);
        for conn in 0..3 {
            let _ = decide_fleet_alert(&mut state, Some(conn), FleetAlertKind::Reject, 100);
        }
        assert_eq!(state.rejecting_count(), 3);
        // Supervisor streaming-recovery path: silent bookkeeping only.
        record_fleet_recovery(&mut state, Some(0));
        assert_eq!(state.rejecting_count(), 2);
        record_fleet_recovery(&mut state, None); // non-fleet no-op
        assert_eq!(state.rejecting_count(), 2);
        // Bridge connected ping 5s after the reject summary: its OWN
        // direction window is untouched → emits. The remainder count is
        // "no reject recorded" = 1 of 3 (only conn 0 recovered; conns 1 + 2
        // are STILL in the reject set — socket-connected is not streaming).
        assert_eq!(
            decide_fleet_alert(&mut state, Some(1), FleetAlertKind::Connected, 105),
            FleetAlertDecision::EmitSummary {
                affected: 1,
                fleet_size: 3
            }
        );
        // A second connected inside the window suppresses.
        assert_eq!(
            decide_fleet_alert(&mut state, Some(2), FleetAlertKind::Connected, 106),
            FleetAlertDecision::Suppress
        );
        // Neither Connected event touched the reject set.
        assert_eq!(state.rejecting_count(), 2);
        // The NEXT reject window therefore still reports the honest
        // accumulated count for the still-rejecting connections.
        assert_eq!(
            decide_fleet_alert(&mut state, Some(1), FleetAlertKind::Reject, 200),
            FleetAlertDecision::EmitSummary {
                affected: 2,
                fleet_size: 3
            }
        );
    }

    /// Scale-down / GROWW-SCALE-01 rollback: the killed (newest) conn ids are
    /// pruned from the reject set when the reconciler publishes the shrunken
    /// fleet size — phantom dead connections can never inflate a later
    /// summary into a false whole-fleet-down claim (hostile-review fix
    /// 2026-07-06).
    #[test]
    fn test_set_fleet_size_prunes_scaled_down_conn_ids() {
        let mut state = FleetAlertState::default();
        state.set_fleet_size(20);
        // The newest 10 connections reject (the exact set a ladder rollback
        // then kills).
        for conn in 10..20 {
            let _ = decide_fleet_alert(&mut state, Some(conn), FleetAlertKind::Reject, 100);
        }
        assert_eq!(state.rejecting_count(), 10);
        // Rollback 20 → 10: reconciler publishes the new size; phantoms gone.
        state.set_fleet_size(10);
        assert_eq!(state.rejecting_count(), 0);
        // A single later hiccup on a LIVE connection reports 1 of 10 — never
        // "11 of 11".
        assert_eq!(
            decide_fleet_alert(&mut state, Some(3), FleetAlertKind::Reject, 200),
            FleetAlertDecision::EmitSummary {
                affected: 1,
                fleet_size: 10
            }
        );
    }

    /// Backwards clock step must widen (never re-open) the window.
    #[test]
    fn test_backwards_clock_never_double_fires() {
        let mut state = FleetAlertState::default();
        state.set_fleet_size(2);
        let _ = decide_fleet_alert(&mut state, Some(0), FleetAlertKind::Reject, 1_000);
        assert_eq!(
            decide_fleet_alert(&mut state, Some(1), FleetAlertKind::Reject, 500),
            FleetAlertDecision::Suppress
        );
    }

    /// Summary wording: the remainder suffix carries ONLY the negative
    /// evidence we actually have ("no reject reported") — never a positive
    /// "streaming normally" health claim (audit-findings Rule 11 / the
    /// connected≠streaming split); no library names, no file paths
    /// (Telegram commandments).
    #[test]
    fn test_format_fleet_reject_summary_wording() {
        let partial = format_fleet_reject_summary(7, 40, "server session limit or throttle");
        assert!(partial.contains("7 of 40 connections retrying"));
        assert!(partial.contains("server session limit or throttle"));
        assert!(partial.contains("no reject reported from the other 33 connections"));
        let full = format_fleet_reject_summary(40, 40, "feed error");
        assert!(full.contains("40 of 40 connections retrying"));
        assert!(
            !full.contains("no reject reported"),
            "no remainder suffix when the whole fleet is down"
        );
        for msg in [&partial, &full] {
            assert!(
                !msg.contains("streaming normally") && !msg.contains("streaming"),
                "positive health claim without positive evidence (false-OK): {msg}"
            );
            assert!(!msg.contains(".rs"), "file path leaked: {msg}");
        }
    }

    /// Fleet size never reported smaller than the affected count even if the
    /// reconciler's publish lags a pass.
    #[test]
    fn test_fleet_size_never_smaller_than_affected() {
        let mut state = FleetAlertState::default();
        // fleet_size still 0 (reconciler hasn't published yet).
        assert_eq!(
            decide_fleet_alert(&mut state, Some(5), FleetAlertKind::Reject, 10),
            FleetAlertDecision::EmitSummary {
                affected: 1,
                fleet_size: 1
            }
        );
    }

    /// The process-wide wrapper: single-conn stays lock-free passthrough;
    /// fleet ids flow through the shared window. ONE test drives the global
    /// so parallel tests never contend on shared state.
    #[test]
    fn test_global_wrapper_end_to_end() {
        assert_eq!(
            global_fleet_alert_decision(None, FleetAlertKind::Reject),
            FleetAlertDecision::Passthrough
        );
        set_global_fleet_size(2);
        let first = global_fleet_alert_decision(Some(90), FleetAlertKind::Reject);
        assert!(
            matches!(first, FleetAlertDecision::EmitSummary { .. }),
            "first fleet reject must summarize, got {first:?}"
        );
        assert_eq!(
            global_fleet_alert_decision(Some(91), FleetAlertKind::Reject),
            FleetAlertDecision::Suppress
        );
        global_record_fleet_recovery(Some(90));
        global_record_fleet_recovery(Some(91));
        assert_eq!(lock_global().rejecting_count(), 0);
    }
}
