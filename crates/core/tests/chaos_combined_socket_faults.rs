//! Combined-fault chaos: the 808 refresh-once state machine under a whole
//! pool of sockets rejected at the 09:15 open (scenario S7).
//!
//! # Why this file exists
//!
//! The inline tests in `pool_supervisor.rs` drive ONE `ConnectionSupervisor`
//! through the 808 rules (refresh once, park on (a) the verified generation
//! rejected again, (b) 3 outright refresh failures, (c) 300 s without a fresh
//! token, (d) more than 4 fresh tokens in 300 s), and one test checks that
//! sixteen first 808s fan out above the 5 s floor. Nothing drove sixteen
//! supervisors THROUGH THE WHOLE EPISODE TOGETHER against one shared token,
//! which is what the open looks like when every socket is rejected at once:
//! the refresh is shared (a socket whose dialled generation was already
//! replaced by a sibling reports `Fresh` without minting; a renewal failure is
//! reused for 10 s as a deferral, `RENEW_FAILURE_REUSE_MS`), so one socket's
//! outcome is another socket's input.
//!
//! # The model
//!
//! Every supervisor is the REAL `ConnectionSupervisor` (public API only:
//! `on_event`, `token_refresh_request`, `note_token_refresh`). The shell is
//! replaced by an event-driven loop that does what `run_connection_with_commands`
//! does for `RefreshTokenThenDial` (sleep the returned delay, refresh, report,
//! then `BeginDial`), on simulated `Instant`s, so 3,600 s of open run in
//! milliseconds. The token is [`SharedToken`], a test model of the app's
//! refresh closure (`dhan_feed_stack.rs`) plus the token manager's 10 s
//! failure reuse: skip when the request says so (`NotAttempted`), `Fresh`
//! when the generation already moved past the dialled one, a reused failure
//! inside 10 s is `Deferred`, otherwise one real mint whose result depends on
//! the scenario. The vendor is a predicate on the presented generation.
//! Honest limit: the mint cooldown is modelled only as "every real attempt
//! defers" (scenario D); the real token manager's cooldown arithmetic is not
//! reproduced here.
//!
//! # Scenarios
//!
//! - **A** stale token at the open, mint works: one shared mint, all sixteen
//!   sockets come back, none parks.
//! - **B** the account itself is refused (a wrong client id answered as 808):
//!   one shared mint, every socket parks on rule (a) at its second 808, no
//!   second mint.
//! - **C** the mint endpoint fails outright: real attempts are at least 10 s
//!   apart across the pool; no socket parks except on rule (b) (its own 3
//!   outright failures) or rule (c) (300 s); every socket parks in bounded
//!   time.
//! - **D** every mint is deferred (cooldown): no socket parks before 300 s
//!   from its episode start; every socket parks within 300 s plus three
//!   redial cycles.
//! - **E** the mint fails for the first 60 s, then works: every socket that
//!   did not reach 3 outright failures comes back; no park outside the rules.
//! - **F** a seeded random mix (vendor accept/reject per dial, accepted
//!   sockets later dropped with 808 or a bare reset, mint ok/fail/defer)
//!   over 300 seeds: the oracle below holds at every park, nothing dials
//!   after a park, and no slot records more than 4 fresh tokens in any
//!   300 s window.
//!
//! The oracle (necessary conditions, read from the source): a park reported by
//! `note_token_refresh` needs the presented generation to equal the one the
//! episode verified (rule a); a park on an 808 close needs rule (b) 3 outright
//! failures, rule (c) 300 s since the episode start or the last fresh token,
//! or rule (d) 4 fresh tokens inside the last 300 s.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use tickvault_core::auth::token_manager::RENEW_FAILURE_REUSE_MS;
use tickvault_core::websocket::pool_budget::{ConnectionSlot, DhanAccount, DhanEndpointType};
use tickvault_core::websocket::pool_supervisor::{
    AUTH_REJECT_EPISODE_MAX_MS, AUTH_REJECT_MAX_HARD_FAILURES,
    AUTH_REJECT_MAX_REFRESHES_PER_WINDOW, AUTH_REJECT_WINDOW_MS, ConnEvent, ConnPhase,
    ConnectionSupervisor, ParkReason, SupervisorAction, TOKEN_STALE_REDIAL_FLOOR_MS,
    TokenRefreshOutcome, TokenRefreshReport, TokenRefreshRequest,
};
use tickvault_core::websocket::types::DisconnectCode;

/// How one real mint attempt ends in a scenario.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mint {
    Ok,
    Fail,
    Defer,
}

/// The shared token: one generation for the whole pool, as in production.
struct SharedToken {
    generation: u64,
    /// The last renewal failure (when, for which dialled generation), reused
    /// as a deferral for 10 s like `RENEW_FAILURE_REUSE_MS`.
    last_failure: Option<(Instant, u64)>,
    /// Successful mints.
    mints: u32,
    /// Instants of real (non-reused) failed or deferred attempts.
    failed_attempts: Vec<Instant>,
}

impl SharedToken {
    fn new() -> Self {
        Self {
            generation: 0,
            last_failure: None,
            mints: 0,
            failed_attempts: Vec::new(),
        }
    }

    /// The app's refresh closure, modelled.
    fn refresh(
        &mut self,
        request: TokenRefreshRequest,
        dialled: u64,
        now: Instant,
        mint: Mint,
    ) -> TokenRefreshReport {
        let report = |outcome| TokenRefreshReport {
            presented: dialled,
            outcome,
        };
        if request.skip_if_presented == Some(dialled) {
            return report(TokenRefreshOutcome::NotAttempted);
        }
        if self.generation != dialled {
            // A sibling already replaced the token this socket dialled with.
            return report(TokenRefreshOutcome::Fresh {
                current: self.generation,
            });
        }
        if let Some((at, generation)) = self.last_failure
            && generation == dialled
            && now.saturating_duration_since(at) < Duration::from_millis(RENEW_FAILURE_REUSE_MS)
        {
            return report(TokenRefreshOutcome::Deferred);
        }
        match mint {
            Mint::Ok => {
                self.generation += 1;
                self.mints += 1;
                self.last_failure = None;
                report(TokenRefreshOutcome::Fresh {
                    current: self.generation,
                })
            }
            Mint::Fail => {
                self.last_failure = Some((now, dialled));
                self.failed_attempts.push(now);
                report(TokenRefreshOutcome::Failed)
            }
            Mint::Defer => {
                self.last_failure = Some((now, dialled));
                self.failed_attempts.push(now);
                report(TokenRefreshOutcome::Deferred)
            }
        }
    }
}

/// Test-side record of one slot's 808 episode, for the oracle.
#[derive(Default)]
struct Oracle {
    episode_start: Option<Instant>,
    last_progress: Option<Instant>,
    hard_failures: u32,
    verified: Option<u64>,
    fresh: Vec<Instant>,
}

impl Oracle {
    fn fresh_in_window(&self, now: Instant) -> usize {
        let window = Duration::from_millis(AUTH_REJECT_WINDOW_MS);
        self.fresh
            .iter()
            .filter(|t| now.saturating_duration_since(**t) < window)
            .count()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    /// Waiting to wake (dial or refresh-then-dial) at `wake`.
    Waiting,
    /// Accepted by the vendor and left streaming: the scenario is done with it.
    Live,
    /// Parked.
    Parked,
}

struct Sock {
    sup: ConnectionSupervisor,
    wake: Instant,
    /// What the shell does when it wakes.
    refresh_first: bool,
    /// Generation the last dial presented.
    presented: u64,
    fate: Fate,
    parked_at: Option<Instant>,
    oracle: Oracle,
    delays: Vec<u64>,
    /// Instants of `Fresh` reports this slot fed back (any episode).
    fresh_log: Vec<Instant>,
}

/// What the vendor does to a dial that presented `generation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// 808 on the handshake.
    Reject808,
    /// Accepted and left streaming.
    Accept,
    /// Accepted, then closed with 808 `after` later with no frame.
    AcceptThen808(Duration),
    /// Accepted, then a bare reset `after` later with no frame.
    AcceptThenReset(Duration),
}

fn all_slots() -> Vec<ConnectionSlot> {
    let mut slots = Vec::new();
    for endpoint in DhanEndpointType::ALL {
        for pool_index in 0..endpoint.max_connections() {
            slots.push(ConnectionSlot {
                account: DhanAccount::Primary,
                endpoint,
                pool_index,
                global_index: endpoint.jitter_base().saturating_add(pool_index),
            });
        }
    }
    slots
}

/// The outcome of one simulated open.
struct Run {
    socks: Vec<Sock>,
    token: SharedToken,
    start: Instant,
    violations: Vec<String>,
}

/// Drives every socket from a first dial at `start` (09:15) until each is
/// live or parked, or `horizon` passes. `vendor(slot, presented, now, dial_no)`
/// and `mint(now)` are the scenario.
fn run_open(
    horizon: Duration,
    mut vendor: impl FnMut(usize, u64, Instant, u32) -> Verdict,
    mut mint: impl FnMut(Instant) -> Mint,
) -> Run {
    let start = Instant::now();
    let mut socks: Vec<Sock> = all_slots()
        .into_iter()
        .map(|slot| Sock {
            sup: ConnectionSupervisor::new(slot, start),
            wake: start,
            refresh_first: false,
            presented: 0,
            fate: Fate::Waiting,
            parked_at: None,
            oracle: Oracle::default(),
            delays: Vec::new(),
            fresh_log: Vec::new(),
        })
        .collect();
    assert_eq!(socks.len(), 16, "the sixteen primary-account slots");
    let mut token = SharedToken::new();
    let mut violations = Vec::new();
    let mut dials = vec![0u32; socks.len()];
    // Pending closes for accepted sockets: (slot, at, code).
    let mut closes: Vec<(usize, Instant, Option<DisconnectCode>)> = Vec::new();
    let end = start + horizon;
    loop {
        // Next event: a socket wake or a scheduled close, earliest first.
        let next_wake = socks
            .iter()
            .enumerate()
            .filter(|(_, s)| s.fate == Fate::Waiting)
            .min_by_key(|(_, s)| s.wake)
            .map(|(i, s)| (i, s.wake));
        let next_close = closes
            .iter()
            .enumerate()
            .min_by_key(|(_, c)| c.1)
            .map(|(k, c)| (k, c.1));
        let (is_close, idx, at) = match (next_wake, next_close) {
            (None, None) => break,
            (Some((i, w)), None) => (false, i, w),
            (None, Some((k, c))) => (true, k, c),
            (Some((i, w)), Some((k, c))) => {
                if c <= w {
                    (true, k, c)
                } else {
                    (false, i, w)
                }
            }
        };
        if at > end {
            break;
        }
        let (i, close_code) = if is_close {
            let (i, _, code) = closes.remove(idx);
            (i, Some(code))
        } else {
            (idx, None)
        };
        let sock = &mut socks[i];
        let action = if let Some(code) = close_code {
            // A previously accepted socket closes now (no frame was delivered).
            sock.fate = Fate::Waiting;
            if code == Some(DisconnectCode::AuthenticationFailed)
                && sock.oracle.episode_start.is_none()
            {
                sock.oracle.episode_start = Some(at);
                sock.oracle.last_progress = Some(at);
            }
            let action = sock.sup.on_event(ConnEvent::Disconnected { code }, at);
            check_close_park(sock, &action, code, at, &mut violations);
            action
        } else {
            // Wake: refresh first when the last action asked for it.
            let mut parked = None;
            if sock.refresh_first {
                let request = sock.sup.token_refresh_request();
                let report = token.refresh(request, sock.presented, at, mint(at));
                if let TokenRefreshOutcome::Fresh { current } = report.outcome {
                    // Fresh only counts when the report does not park.
                    let _ = current;
                }
                let verified_before = sock.oracle.verified;
                parked = sock.sup.note_token_refresh(report, at);
                if let Some(p) = &parked {
                    if !matches!(p, SupervisorAction::Park { .. }) {
                        violations.push(format!("slot {i}: a refresh report returned {p:?}"));
                    }
                    if verified_before != Some(report.presented) {
                        violations.push(format!(
                            "slot {i}: parked on a refresh report although the presented \
                             generation {} was not the verified {:?} (rule a)",
                            report.presented, verified_before
                        ));
                    }
                } else {
                    match report.outcome {
                        TokenRefreshOutcome::Fresh { current } => {
                            sock.oracle.verified = Some(current);
                            sock.oracle.last_progress = Some(at);
                            sock.oracle.fresh.push(at);
                            sock.fresh_log.push(at);
                        }
                        TokenRefreshOutcome::Failed | TokenRefreshOutcome::NotAttempted => {
                            sock.oracle.hard_failures += 1;
                        }
                        TokenRefreshOutcome::Deferred => {}
                    }
                }
            }
            match parked {
                Some(park) => park,
                None => {
                    let dial = sock.sup.on_event(ConnEvent::BeginDial, at);
                    if dial != SupervisorAction::Dial {
                        violations.push(format!("slot {i}: BeginDial returned {dial:?}"));
                        sock.fate = Fate::Parked;
                        continue;
                    }
                    sock.presented = token.generation;
                    dials[i] += 1;
                    let _ = sock.sup.on_event(ConnEvent::DialSucceeded, at);
                    match vendor(i, sock.presented, at, dials[i]) {
                        Verdict::Reject808 => {
                            if sock.oracle.episode_start.is_none() {
                                sock.oracle.episode_start = Some(at);
                                sock.oracle.last_progress = Some(at);
                            }
                            let code = Some(DisconnectCode::AuthenticationFailed);
                            let action = sock.sup.on_event(ConnEvent::Disconnected { code }, at);
                            check_close_park(sock, &action, code, at, &mut violations);
                            action
                        }
                        Verdict::Accept => {
                            let _ = sock.sup.on_event(ConnEvent::SubscribeAcked, at);
                            sock.fate = Fate::Live;
                            continue;
                        }
                        Verdict::AcceptThen808(after) => {
                            let _ = sock.sup.on_event(ConnEvent::SubscribeAcked, at);
                            sock.fate = Fate::Live;
                            closes.push((
                                i,
                                at + after,
                                Some(DisconnectCode::AuthenticationFailed),
                            ));
                            continue;
                        }
                        Verdict::AcceptThenReset(after) => {
                            let _ = sock.sup.on_event(ConnEvent::SubscribeAcked, at);
                            sock.fate = Fate::Live;
                            closes.push((i, at + after, None));
                            continue;
                        }
                    }
                }
            }
        };
        let sock = &mut socks[i];
        match action {
            SupervisorAction::RefreshTokenThenDial { delay_ms } => {
                sock.refresh_first = true;
                sock.wake = at + Duration::from_millis(delay_ms);
                sock.delays.push(delay_ms);
                sock.fate = Fate::Waiting;
            }
            SupervisorAction::SleepThenDial { delay_ms } => {
                sock.refresh_first = false;
                sock.wake = at + Duration::from_millis(delay_ms);
                sock.delays.push(delay_ms);
                sock.fate = Fate::Waiting;
            }
            SupervisorAction::Park { reason } => {
                if reason != ParkReason::FatalDisconnect {
                    violations.push(format!(
                        "slot {i}: parked for {reason:?}, not FatalDisconnect"
                    ));
                }
                sock.fate = Fate::Parked;
                sock.parked_at = Some(at);
                // Nothing dials after a park.
                let again = sock
                    .sup
                    .on_event(ConnEvent::BeginDial, at + Duration::from_secs(1));
                if again == SupervisorAction::Dial {
                    violations.push(format!("slot {i}: dialled after a park"));
                }
                if sock.sup.phase() != ConnPhase::Parked {
                    violations.push(format!(
                        "slot {i}: park action but phase {:?}",
                        sock.sup.phase()
                    ));
                }
            }
            other => {
                violations.push(format!("slot {i}: unexpected action {other:?}"));
                sock.fate = Fate::Parked;
            }
        }
    }
    // Rule (d), checked on the record: never more than 4 fresh tokens charged
    // to one slot inside any 300 s window (only a non-parking Fresh counts).
    for (i, sock) in socks.iter().enumerate() {
        let window = Duration::from_millis(AUTH_REJECT_WINDOW_MS);
        for (k, t) in sock.fresh_log.iter().enumerate() {
            let in_window = sock.fresh_log[k..]
                .iter()
                .filter(|u| u.saturating_duration_since(*t) < window)
                .count();
            if in_window > AUTH_REJECT_MAX_REFRESHES_PER_WINDOW {
                violations.push(format!(
                    "slot {i}: {in_window} fresh tokens inside one 300 s window"
                ));
            }
        }
    }
    Run {
        socks,
        token,
        start,
        violations,
    }
}

/// The oracle for a park on an 808 close: rule (b), (c) or (d) must hold.
fn check_close_park(
    sock: &Sock,
    action: &SupervisorAction,
    code: Option<DisconnectCode>,
    at: Instant,
    violations: &mut Vec<String>,
) {
    if !matches!(action, SupervisorAction::Park { .. }) {
        return;
    }
    if code != Some(DisconnectCode::AuthenticationFailed) {
        violations.push(format!("parked on a non-808 close {code:?}: {action:?}"));
        return;
    }
    let rule_b = sock.oracle.hard_failures >= u32::from(AUTH_REJECT_MAX_HARD_FAILURES);
    let rule_c = sock.oracle.last_progress.is_some_and(|p| {
        at.saturating_duration_since(p) >= Duration::from_millis(AUTH_REJECT_EPISODE_MAX_MS)
    });
    let rule_d = sock.oracle.fresh_in_window(at) >= AUTH_REJECT_MAX_REFRESHES_PER_WINDOW;
    if !(rule_b || rule_c || rule_d) {
        violations.push(format!(
            "parked on an 808 with no documented condition: hard {} since_progress {:?} \
             fresh_in_window {}",
            sock.oracle.hard_failures,
            sock.oracle
                .last_progress
                .map(|p| at.saturating_duration_since(p)),
            sock.oracle.fresh_in_window(at)
        ));
    }
}

fn max_delay(run: &Run) -> Duration {
    Duration::from_millis(
        run.socks
            .iter()
            .flat_map(|s| s.delays.iter().copied())
            .max()
            .unwrap_or(0),
    )
}

const HOUR: Duration = Duration::from_secs(3_600);

/// A: a stale token at the open, the mint works. One shared mint; all sixteen
/// come back; none parks; every first redial waits at least the 5 s floor.
#[test]
fn s7a_sixteen_808s_at_the_open_share_one_mint_and_all_recover() {
    let run = run_open(
        HOUR,
        |_, presented, _, _| {
            if presented == 0 {
                Verdict::Reject808
            } else {
                Verdict::Accept
            }
        },
        |_| Mint::Ok,
    );
    assert!(run.violations.is_empty(), "{:#?}", run.violations);
    assert_eq!(run.token.mints, 1, "one shared mint for sixteen 808s");
    for (i, s) in run.socks.iter().enumerate() {
        assert_eq!(s.fate, Fate::Live, "slot {i} came back");
        assert!(
            s.delays.first().copied().unwrap_or(0) >= TOKEN_STALE_REDIAL_FLOOR_MS,
            "slot {i} waited the token floor"
        );
    }
}

/// B: the account itself is refused. One mint, then every socket parks on
/// rule (a) at its second 808; nobody mints again.
#[test]
fn s7b_account_level_808_parks_every_socket_on_rule_a_after_one_mint() {
    let run = run_open(HOUR, |_, _, _, _| Verdict::Reject808, |_| Mint::Ok);
    assert!(run.violations.is_empty(), "{:#?}", run.violations);
    assert_eq!(run.token.mints, 1, "the verified token is never replaced");
    for (i, s) in run.socks.iter().enumerate() {
        assert_eq!(s.fate, Fate::Parked, "slot {i} parked");
        let parked_after = s.parked_at.unwrap().saturating_duration_since(run.start);
        assert!(
            parked_after < Duration::from_secs(60),
            "slot {i} parked after {parked_after:?}: rule (a) is the second 808, not a timeout"
        );
        assert_eq!(
            s.oracle.hard_failures, 0,
            "slot {i} parked on rule (a), not on failures"
        );
    }
}

/// C: the mint endpoint fails outright for the whole hour. The 10 s reuse
/// keeps real attempts at least 10 s apart; parks only on (b) or (c); every
/// socket parks in bounded time.
#[test]
fn s7c_mint_failing_outright_parks_only_on_rules_b_or_c_and_attempts_are_spaced() {
    let run = run_open(HOUR, |_, _, _, _| Verdict::Reject808, |_| Mint::Fail);
    assert!(run.violations.is_empty(), "{:#?}", run.violations);
    assert_eq!(run.token.mints, 0);
    let reuse = Duration::from_millis(RENEW_FAILURE_REUSE_MS);
    for pair in run.token.failed_attempts.windows(2) {
        assert!(
            pair[1].saturating_duration_since(pair[0]) >= reuse,
            "two real renewals inside the 10 s reuse window"
        );
    }
    let bound = Duration::from_millis(AUTH_REJECT_EPISODE_MAX_MS) + 3 * max_delay(&run);
    for (i, s) in run.socks.iter().enumerate() {
        assert_eq!(s.fate, Fate::Parked, "slot {i} parked");
        let after = s.parked_at.unwrap().saturating_duration_since(run.start);
        assert!(
            after <= bound,
            "slot {i} parked after {after:?} > {bound:?}"
        );
    }
    // Non-vacuity: the run reached BOTH branches. The sockets that made the
    // real attempts struck out on rule (b); the ones that only read the
    // reused failure back were deferred and parked on rule (c).
    let struck = run
        .socks
        .iter()
        .filter(|s| s.oracle.hard_failures >= u32::from(AUTH_REJECT_MAX_HARD_FAILURES))
        .count();
    let deferred_only = run
        .socks
        .iter()
        .filter(|s| s.oracle.hard_failures < u32::from(AUTH_REJECT_MAX_HARD_FAILURES))
        .count();
    let attempts = run.token.failed_attempts.len();
    eprintln!("s7c: struck {struck}, deferred-only {deferred_only}, real attempts {attempts}");
    assert!(struck >= 1, "rule (b) was reached");
    assert!(deferred_only >= 1, "rule (c) was reached");
    // One real attempt per 10 s window at most, across the whole pool.
    let span = run
        .token
        .failed_attempts
        .last()
        .map_or(Duration::ZERO, |l| {
            l.saturating_duration_since(run.token.failed_attempts[0])
        });
    let reuse_secs = RENEW_FAILURE_REUSE_MS / 1_000;
    assert!(u64::try_from(attempts).unwrap() <= span.as_secs() / reuse_secs + 1);
}

/// D: every real attempt is deferred (mint cooldown / RESILIENCE-03). A
/// deferral never parks early: nothing parks before 300 s from its episode
/// start, and everything parks by 300 s plus three redial cycles.
#[test]
fn s7d_deferrals_never_park_a_socket_before_300_s_and_all_park_by_the_bound() {
    let run = run_open(HOUR, |_, _, _, _| Verdict::Reject808, |_| Mint::Defer);
    assert!(run.violations.is_empty(), "{:#?}", run.violations);
    let episode = Duration::from_millis(AUTH_REJECT_EPISODE_MAX_MS);
    let bound = episode + 3 * max_delay(&run);
    for (i, s) in run.socks.iter().enumerate() {
        assert_eq!(s.fate, Fate::Parked, "slot {i} parked");
        let start = s.oracle.episode_start.unwrap();
        let after = s.parked_at.unwrap().saturating_duration_since(start);
        assert!(after >= episode, "slot {i} parked early, after {after:?}");
        assert!(
            after <= bound,
            "slot {i} parked late, after {after:?} > {bound:?}"
        );
        assert_eq!(s.oracle.hard_failures, 0, "a deferral is never a strike");
    }
}

/// E: the mint fails for the first 60 s, then works. Every socket that did
/// not reach 3 outright failures before 60 s comes back.
#[test]
fn s7e_mint_recovering_after_60_s_brings_back_every_socket_not_struck_out() {
    let mut first: Option<Instant> = None;
    let run = run_open(
        HOUR,
        |_, presented, _, _| {
            if presented == 0 {
                Verdict::Reject808
            } else {
                Verdict::Accept
            }
        },
        move |now| {
            let t0 = *first.get_or_insert(now);
            if now.saturating_duration_since(t0) < Duration::from_secs(60) {
                Mint::Fail
            } else {
                Mint::Ok
            }
        },
    );
    assert!(run.violations.is_empty(), "{:#?}", run.violations);
    assert_eq!(
        run.token.mints, 1,
        "one shared mint once the endpoint heals"
    );
    for (i, s) in run.socks.iter().enumerate() {
        if s.oracle.hard_failures < u32::from(AUTH_REJECT_MAX_HARD_FAILURES) {
            assert_eq!(s.fate, Fate::Live, "slot {i} came back");
        }
    }
    let live = run.socks.iter().filter(|s| s.fate == Fate::Live).count();
    assert!(live >= 1, "the pool recovers");
}

/// Small deterministic generator (no new dependency, seeds are replayable).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// F: a seeded random mix over 300 seeds. The oracle holds at every park,
/// nothing dials after a park, rule (d) holds on the record.
#[test]
fn s7f_random_vendor_and_mint_mixes_never_park_outside_the_documented_rules() {
    let mut parks = 0usize;
    let mut lives = 0usize;
    for seed in 0..300u64 {
        let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5);
        let mut mint_rng = Lcg(seed ^ 0x5DEE_CE66);
        let accept_pct = rng.below(70);
        let run = run_open(
            HOUR,
            move |_, _, _, dial_no| {
                // Bounded so every run terminates: past 40 dials a slot is
                // refused, which the rules must still park.
                if dial_no > 40 {
                    return Verdict::Reject808;
                }
                let roll = rng.below(100);
                if roll < accept_pct / 3 {
                    Verdict::Accept
                } else if roll < accept_pct {
                    let after = Duration::from_millis(rng.below(25_000));
                    if rng.below(2) == 0 {
                        Verdict::AcceptThen808(after)
                    } else {
                        Verdict::AcceptThenReset(after)
                    }
                } else {
                    Verdict::Reject808
                }
            },
            move |_| match mint_rng.below(3) {
                0 => Mint::Ok,
                1 => Mint::Fail,
                _ => Mint::Defer,
            },
        );
        assert!(
            run.violations.is_empty(),
            "seed {seed}: {:#?}",
            run.violations
        );
        parks += run.socks.iter().filter(|s| s.fate == Fate::Parked).count();
        lives += run.socks.iter().filter(|s| s.fate == Fate::Live).count();
    }
    // Non-vacuity: the mix reached both ends.
    assert!(parks > 100, "only {parks} parks across 300 seeds");
    assert!(lives > 100, "only {lives} live sockets across 300 seeds");
}
