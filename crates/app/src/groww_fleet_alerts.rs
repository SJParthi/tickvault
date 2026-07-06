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
//!   count summary ("7 of 40 connections retrying … — others streaming
//!   normally").
//!
//! # Honest envelope
//!
//! The summary is edge-triggered (audit-findings Rule 4): the FIRST event of
//! a window emits with the count known AT THAT INSTANT; connections that
//! join the reject set later in the same window are counted by the NEXT
//! window's summary (the reject set persists across windows). There is
//! deliberately NO delayed/debounced flush task — no timer, no new
//! background task, no new silent-death surface. Per-connection `error!`
//! logs, feed-health state, and `ws_event_audit` forensic rows are NEVER
//! coalesced — only the operator-facing Telegram fan-out is.

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
        /// currently NOT rejecting (Connected direction).
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
    /// pass). Never shrinks the reject set — a scaled-down conn id simply
    /// stops producing events and is cleared on its next recovery edge.
    pub fn set_fleet_size(&mut self, n: usize) {
        self.fleet_size = n;
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
            state.rejecting.remove(&conn);
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
        // Connected direction: the healthy remainder (at least the reporting
        // connection itself, so the count is never a misleading 0).
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
#[must_use]
pub fn format_fleet_reject_summary(affected: usize, fleet_size: usize, label: &str) -> String {
    if affected >= fleet_size {
        format!("{affected} of {fleet_size} connections retrying ({label})")
    } else {
        format!(
            "{affected} of {fleet_size} connections retrying ({label}) — \
             others streaming normally"
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

    /// Recovery edge: connected transitions clear the reject set; the
    /// Connected direction has its OWN window so a recovery right after a
    /// reject summary still reaches the operator, and the supervisor's
    /// silent recovery bookkeeping never consumes the bridge's window.
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
        // direction window is untouched → emits. The Connected event itself
        // clears conn 1 from the reject set BEFORE counting, so the healthy
        // count is 2 of 3 (conns 0 + 1 recovered; conn 2 still rejecting).
        assert_eq!(
            decide_fleet_alert(&mut state, Some(1), FleetAlertKind::Connected, 105),
            FleetAlertDecision::EmitSummary {
                affected: 2,
                fleet_size: 3
            }
        );
        // A second connected inside the window suppresses.
        assert_eq!(
            decide_fleet_alert(&mut state, Some(2), FleetAlertKind::Connected, 106),
            FleetAlertDecision::Suppress
        );
        // Both connected events still cleared their reject entries.
        assert_eq!(state.rejecting_count(), 0);
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

    /// Summary wording: honest suffix only when some connections ARE healthy;
    /// no library names, no file paths (Telegram commandments).
    #[test]
    fn test_format_fleet_reject_summary_wording() {
        let partial = format_fleet_reject_summary(7, 40, "server session limit or throttle");
        assert!(partial.contains("7 of 40 connections retrying"));
        assert!(partial.contains("server session limit or throttle"));
        assert!(partial.contains("others streaming normally"));
        let full = format_fleet_reject_summary(40, 40, "feed error");
        assert!(full.contains("40 of 40 connections retrying"));
        assert!(
            !full.contains("others streaming normally"),
            "no false 'others streaming' when the whole fleet is down"
        );
        for msg in [&partial, &full] {
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
