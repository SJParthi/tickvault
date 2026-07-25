//! TrueData (feed #4) session state machine + connect-URL construction.
//!
//! **Ground truth:** `TrueData Market Data API Documentation v 2.6`
//! pages 9, 10, 18 and 19, read directly.
//!
//! # The problem this solves
//!
//! TrueData permits **exactly one login per key** (v2.6 p.10: *"If the
//! client is connected elsewhere, the client would not be able to
//! connect"*, refusal message `User Already Connected`). Recovering from a
//! dirty disconnect requires the force-logout HTTP call followed by a
//! **1-minute wait** before the next login can succeed (v2.6 p.19:
//! *"Wait for 1 min after firing the URL and then try to log in"*).
//!
//! That creates a race that an ordinary reconnect ladder gets WRONG: the
//! backoff timer and the force-logout wait are independent, so a reconnect
//! attempt fires DURING the cooldown, is refused with `User Already
//! Connected`, is misread as fresh lock contention, triggers ANOTHER
//! force-logout, and the lane never recovers — feed down for the session.
//!
//! The fix is to make them ONE state machine with a single clock, where
//! [`SessionState::CoolingDown`] HARD-REFUSES connect attempts instead of
//! letting a second timer race it. That is the whole point of this module.
//!
//! # Credentials
//!
//! Auth is user + password in the connect-URL query string (v2.6 p.9),
//! so the URL itself is a secret. [`TruedataConnectUrl`] therefore has NO
//! `Display`/`Debug` that can leak it: the only printable form is
//! [`TruedataConnectUrl::redacted`], and the raw value is reachable only
//! through an explicitly-named accessor.

use core::fmt;
use core::time::Duration;

/// Sandbox real-time port (v2.6 p.9).
pub const TD_PORT_SANDBOX: u16 = 8086;
/// Production real-time port (v2.6 p.9).
pub const TD_PORT_PRODUCTION: u16 = 8084;

/// Mandatory wait, in seconds, after firing the force-logout URL before
/// the next login can succeed (v2.6 p.19, verbatim: "Wait for 1 min").
pub const TD_FORCE_LOGOUT_COOLDOWN_SECS: u64 = 60;
/// [`TD_FORCE_LOGOUT_COOLDOWN_SECS`] as a `Duration`.
pub const TD_FORCE_LOGOUT_COOLDOWN: Duration = Duration::from_secs(TD_FORCE_LOGOUT_COOLDOWN_SECS);

/// First reconnect backoff step, in seconds.
pub const TD_RECONNECT_BACKOFF_MIN_SECS: u64 = 5;
/// Backoff ceiling in seconds — the ladder never waits longer than this.
pub const TD_RECONNECT_BACKOFF_MAX_SECS: u64 = 60;
/// [`TD_RECONNECT_BACKOFF_MIN_SECS`] as a `Duration`.
pub const TD_RECONNECT_BACKOFF_MIN: Duration = Duration::from_secs(TD_RECONNECT_BACKOFF_MIN_SECS);
/// [`TD_RECONNECT_BACKOFF_MAX_SECS`] as a `Duration`.
pub const TD_RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(TD_RECONNECT_BACKOFF_MAX_SECS);

/// The vendor's duplicate-login refusal message (v2.6 p.10, verbatim).
pub const TD_MSG_ALREADY_CONNECTED: &str = "User Already Connected";

/// The distinctive core of the refusal, matched case-insensitively.
///
/// Matching the FULL literal exactly is brittle: the wire text is
/// UNVERIFIED-LIVE, and any casing or surrounding-wording drift would send
/// a ghost-session refusal down the ordinary backoff ladder — we would
/// never force-logout, and the ghost would hold the single login silently
/// for the whole session.
pub const TD_MSG_ALREADY_CONNECTED_CORE: &str = "already connected";

/// How many force-logouts one session may fire before parking.
///
/// The ghost-clearing cycle (refused → force-logout → 1-minute wait →
/// retry) must terminate. If a real peer holds the key, evicting them every
/// minute just means we evict each other forever and neither side keeps a
/// session. After this many attempts the machine parks and pages instead.
pub const TD_MAX_FORCE_LOGOUTS_PER_SESSION: u32 = 3;

/// Case-insensitive ASCII substring test, allocation-free.
///
/// `to_lowercase()` would allocate; this is used on an auth-refusal path
/// that a hostile or flapping server can drive repeatedly.
#[must_use]
pub fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() || n.len() > h.len() {
        return false;
    }
    // O(1) EXEMPT: begin
    // Substring search inherently compares bytes. Both operands are bounded
    // by a vendor auth message (tens of bytes) and a compile-time literal;
    // this is the cold connect path, never the per-tick hot path.
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
    // O(1) EXEMPT: end
}

// ---------------------------------------------------------------------------
// Connect URL (secret-bearing)
// ---------------------------------------------------------------------------

/// A TrueData connect URL with credentials embedded in the query string.
///
/// Deliberately NOT `Debug`, NOT `Display`, NOT `Clone`-into-logs: the
/// password is IN the string (v2.6 p.9), so any accidental formatting
/// would leak it. Use [`Self::redacted`] for logs; the raw form requires
/// calling [`Self::expose_secret_url`], whose name makes the leak
/// deliberate and greppable.
pub struct TruedataConnectUrl {
    /// Full URL INCLUDING credentials. The host occupies
    /// `url[URL_SCHEME_LEN..URL_SCHEME_LEN + host_len]`, so the host is
    /// sliced out rather than stored a second time — one allocation for
    /// the whole type.
    url: String,
    host_len: usize,
    port: u16,
}

/// Byte length of the `wss://` scheme prefix — the offset at which the
/// host begins inside [`TruedataConnectUrl::url`].
const URL_SCHEME_LEN: usize = 6;

impl TruedataConnectUrl {
    /// Builds `wss://<host>:<port>?user=<user>&password=<password>`.
    ///
    /// The caller supplies credentials read from SSM as `Secret<String>`;
    /// this type takes them by reference and never stores them separately.
    #[must_use]
    pub fn new(host: &str, port: u16, user: &str, password: &str) -> Self {
        let mut url = String::with_capacity(
            host.len()
                .saturating_add(user.len())
                .saturating_add(password.len())
                .saturating_add(48),
        );
        url.push_str("wss://");
        url.push_str(host);
        url.push(':');
        // Small integer; write without allocating a second String.
        let mut buf = itoa_u16(port);
        url.push_str(buf.as_str());
        url.push_str("?user=");
        push_query_escaped(&mut url, user);
        // The query PARAMETER NAME, not a value; the credential arrives from
        // SSM and is escaped below.
        url.push_str("&password="); // secret-scan-ignore: param name, not a secret
        push_query_escaped(&mut url, password);
        buf.clear();
        Self {
            url,
            host_len: host.len(),
            port,
        }
    }

    /// The URL with the password replaced by `<redacted>` — the ONLY form
    /// that may appear in a log, span field, or error message.
    #[must_use]
    pub fn redacted(&self) -> String {
        let mut out = String::with_capacity(self.host_len.saturating_add(48));
        out.push_str("wss://");
        out.push_str(self.host());
        out.push(':');
        out.push_str(itoa_u16(self.port).as_str());
        // The redaction template — the literal that REPLACES credentials.
        out.push_str("?user=<redacted>&password=<redacted>"); // secret-scan-ignore: redaction template
        out
    }

    /// Host only — safe to log.
    #[must_use]
    pub fn host(&self) -> &str {
        self.url
            .get(URL_SCHEME_LEN..URL_SCHEME_LEN.saturating_add(self.host_len))
            .unwrap_or("")
    }

    /// Port only — safe to log.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// The raw URL INCLUDING credentials.
    ///
    /// Named to make every call site obvious in review and greppable in a
    /// source scan. Pass it to the WebSocket connector and nowhere else —
    /// never to a log macro, never into an error string.
    #[must_use]
    pub fn expose_secret_url(&self) -> &str {
        &self.url
    }
}

/// A tiny stack buffer for `u16` → decimal, avoiding a `format!`.
struct SmallNum {
    buf: [u8; 5],
    len: usize,
}

impl SmallNum {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("0")
    }
    fn clear(&mut self) {
        self.len = 0;
    }
}

/// Formats a `u16` into a stack buffer — no heap allocation.
fn itoa_u16(mut value: u16) -> SmallNum {
    let mut tmp = [0u8; 5];
    let mut i = 5usize;
    if value == 0 {
        i = 4;
        tmp[4] = b'0';
    }
    while value > 0 && i > 0 {
        i = i.saturating_sub(1);
        // APPROVED: value > 0 here, so the remainder is a valid digit
        #[allow(clippy::arithmetic_side_effects)]
        {
            tmp[i] = b'0' + (value % 10) as u8;
        }
        value /= 10;
    }
    let len = 5usize.saturating_sub(i);
    let mut buf = [0u8; 5];
    buf[..len].copy_from_slice(&tmp[i..5]);
    SmallNum { buf, len }
}

/// Percent-escapes the few characters that would break a query string.
///
/// TrueData user ids and passwords are not documented to contain these,
/// but a credential that did would otherwise silently truncate the URL
/// and produce a baffling auth failure.
fn push_query_escaped(out: &mut String, raw: &str) {
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("%26"),
            '=' => out.push_str("%3D"),
            '?' => out.push_str("%3F"),
            '#' => out.push_str("%23"),
            ' ' => out.push_str("%20"),
            c => out.push(c),
        }
    }
}

// ---------------------------------------------------------------------------
// Session state machine
// ---------------------------------------------------------------------------

/// Why a connect attempt was refused by the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectRefusal {
    /// The force-logout budget is spent and a ghost still holds the login.
    ///
    /// Terminal for this session — something outside this process owns the
    /// key, and more attempts cannot fix it. The operator is paged.
    GhostBudgetExhausted,
    /// A force-logout is in its mandatory cooldown; connecting now would
    /// be refused by the vendor and would restart the whole cycle.
    CoolingDown {
        /// Seconds still to wait.
        remaining_secs: u64,
    },
    /// The backoff timer has not elapsed yet.
    Backoff {
        /// Seconds still to wait.
        remaining_secs: u64,
    },
    /// The single-session lock is not held, so connecting could fight a
    /// peer process for the one allowed login.
    LockNotHeld,
    /// Already connected — a second connect would be self-inflicted
    /// duplicate-login.
    AlreadyConnected,
}

/// Lifecycle of one TrueData session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Not connected, no timer pending — connect immediately.
    Idle,
    /// A connect attempt is IN FLIGHT (socket opening / authenticating).
    ///
    /// Without this state the machine sits in `Idle` for the whole
    /// handshake, so a second caller — a retry timer firing under a slow
    /// TLS negotiation, or a second in-process task — gets `Ok` again and
    /// opens a SECOND socket. The vendor allows one login per key and
    /// kills the first, so we would be knocking ourselves offline. The SSM
    /// lock only guards against other PROCESSES; this guards against us.
    Connecting,
    /// Connected and authenticated.
    Connected,
    /// Waiting out a reconnect backoff step.
    Backoff {
        /// Elapsed seconds within the current wait.
        waited_secs: u64,
        /// Total seconds this step must wait.
        step_secs: u64,
        /// Consecutive failure count driving the ladder.
        attempt: u32,
    },
    /// Force-logout fired; the vendor's mandatory wait is running.
    ///
    /// Connect attempts are HARD-REFUSED here. This is the state that
    /// prevents the reconnect-storm race described in the module docs.
    CoolingDown {
        /// Elapsed seconds of the cooldown.
        waited_secs: u64,
    },
    /// The force-logout budget is spent and a ghost still holds the login.
    ///
    /// A terminal state on purpose. Something outside this process owns the
    /// key; hammering the vendor cannot fix it, so the machine parks and
    /// the operator is paged.
    ParkedGhost {
        /// How many force-logouts were fired before parking.
        force_logouts: u32,
    },
}

/// The session driver: one clock, one state, no racing timers.
#[derive(Debug, Clone)]
pub struct TruedataSession {
    state: SessionState,
    lock_held: bool,
    /// Force-logouts fired so far in this session.
    ///
    /// Deliberately a SESSION field rather than a `CoolingDown` payload:
    /// the cooldown expiring transitions to `Idle`, which would DISCARD a
    /// state-carried count and reset the budget to zero on every cycle —
    /// the ghost loop would still be unbounded, just less obviously.
    force_logouts: u32,
}

impl Default for TruedataSession {
    fn default() -> Self {
        Self::new()
    }
}

impl TruedataSession {
    /// A fresh session: idle, lock not yet acquired.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: SessionState::Idle,
            lock_held: false,
            force_logouts: 0,
        }
    }

    /// Current state.
    #[must_use]
    pub const fn state(&self) -> SessionState {
        self.state
    }

    /// Records whether the SSM single-session lock is currently held.
    ///
    /// Fail-closed: with the lock lost, [`Self::may_connect`] refuses, so
    /// a lock-lost process never fights its peer for the one login.
    pub const fn set_lock_held(&mut self, held: bool) {
        self.lock_held = held;
    }

    /// `true` when the single-session lock is held.
    #[must_use]
    pub const fn lock_held(&self) -> bool {
        self.lock_held
    }

    /// May a connect attempt fire right now?
    ///
    /// `Ok(())` means go. `Err(refusal)` names the reason so the caller
    /// can log it once rather than hammering the endpoint.
    ///
    /// # Errors
    /// A [`ConnectRefusal`] describing why the attempt is not allowed yet.
    pub const fn may_connect(&self) -> Result<(), ConnectRefusal> {
        match self.state {
            SessionState::ParkedGhost { .. } => Err(ConnectRefusal::GhostBudgetExhausted),
            SessionState::CoolingDown { waited_secs } => Err(ConnectRefusal::CoolingDown {
                remaining_secs: TD_FORCE_LOGOUT_COOLDOWN
                    .as_secs()
                    .saturating_sub(waited_secs),
            }),
            SessionState::Backoff {
                waited_secs,
                step_secs,
                ..
            } if waited_secs < step_secs => Err(ConnectRefusal::Backoff {
                remaining_secs: step_secs.saturating_sub(waited_secs),
            }),
            SessionState::Connected | SessionState::Connecting => {
                Err(ConnectRefusal::AlreadyConnected)
            }
            _ if !self.lock_held => Err(ConnectRefusal::LockNotHeld),
            _ => Ok(()),
        }
    }

    /// Advances every timer by `secs`.
    ///
    /// A single tick drives BOTH the backoff and the cooldown, which is
    /// precisely why they cannot race each other.
    pub const fn tick(&mut self, secs: u64) {
        self.state = match self.state {
            SessionState::CoolingDown { waited_secs } => {
                let waited = waited_secs.saturating_add(secs);
                if waited >= TD_FORCE_LOGOUT_COOLDOWN.as_secs() {
                    SessionState::Idle
                } else {
                    SessionState::CoolingDown {
                        waited_secs: waited,
                    }
                }
            }
            SessionState::Backoff {
                waited_secs,
                step_secs,
                attempt,
            } => SessionState::Backoff {
                waited_secs: waited_secs.saturating_add(secs),
                step_secs,
                attempt,
            },
            other => other,
        };
    }

    /// Marks a connect attempt as STARTED.
    ///
    /// Call this the moment the socket open is initiated, so a concurrent
    /// caller sees `Connecting` and is refused instead of opening a second
    /// socket on the same one-login key.
    pub const fn on_connect_started(&mut self) {
        match self.state {
            // The cooldown and the parked state outrank everything.
            SessionState::CoolingDown { .. } | SessionState::ParkedGhost { .. } => {}
            _ => self.state = SessionState::Connecting,
        }
    }

    /// Marks the session authenticated.
    ///
    /// A no-op during the cooldown or once parked: a stale success
    /// callback from a socket that was already being torn down must not
    /// cancel the vendor's mandatory wait.
    pub const fn on_connected(&mut self) {
        match self.state {
            SessionState::CoolingDown { .. } | SessionState::ParkedGhost { .. } => {}
            _ => self.state = SessionState::Connected,
        }
    }

    /// Records a connection/auth failure and schedules the next backoff.
    ///
    /// The ladder doubles from [`TD_RECONNECT_BACKOFF_MIN`] and saturates
    /// at [`TD_RECONNECT_BACKOFF_MAX`] — bounded, never a tight loop.
    /// Records a connection/auth failure and schedules the next backoff.
    ///
    /// The ladder doubles from [`TD_RECONNECT_BACKOFF_MIN`] and saturates
    /// at [`TD_RECONNECT_BACKOFF_MAX`] — bounded, never a tight loop.
    ///
    /// # The cooldown outranks this
    ///
    /// While [`SessionState::CoolingDown`] (or parked) this is a NO-OP, and
    /// that is the whole point. The natural ordering after a
    /// `User Already Connected` refusal is: refusal → force-logout →
    /// cooldown → **and then the socket closes**, because we were just
    /// refused. That close calls this method. If it overwrote the cooldown
    /// with a 5-second backoff, we would log in again 5 seconds into the
    /// vendor's mandatory 1-minute wait, be refused, force-logout again —
    /// a ~5s reconnect storm for the entire session, hammering the vendor's
    /// force-logout endpoint hundreds of times an hour and never obtaining
    /// the one login. Exactly the bug this module exists to prevent.
    pub const fn on_disconnected(&mut self) {
        let attempt = match self.state {
            // The cooldown and the parked state are not overwritable.
            SessionState::CoolingDown { .. } | SessionState::ParkedGhost { .. } => return,
            SessionState::Backoff { attempt, .. } => attempt.saturating_add(1),
            _ => 0,
        };
        self.state = SessionState::Backoff {
            waited_secs: 0,
            step_secs: backoff_step_secs(attempt),
            attempt,
        };
    }

    /// Records that a force-logout was fired, entering the mandatory
    /// cooldown.
    ///
    /// This SUPERSEDES any pending backoff: after a force-logout the only
    /// thing that matters is the vendor's 1-minute wait, and allowing a
    /// shorter backoff to fire first is exactly the bug this module
    /// exists to prevent.
    /// Records that a force-logout was fired, entering the mandatory
    /// cooldown — or parking when the budget is spent.
    ///
    /// SUPERSEDES any pending backoff: after a force-logout the only thing
    /// that matters is the vendor's 1-minute wait.
    ///
    /// The count is carried so the ghost-clearing loop terminates. After
    /// [`TD_MAX_FORCE_LOGOUTS_PER_SESSION`] attempts the machine parks
    /// instead of cycling forever — if something outside this process owns
    /// the key, evicting it every minute only means neither side ever
    /// keeps a session.
    pub const fn on_force_logout_fired(&mut self) {
        self.force_logouts = self.force_logouts.saturating_add(1);
        self.state = if self.force_logouts >= TD_MAX_FORCE_LOGOUTS_PER_SESSION {
            SessionState::ParkedGhost {
                force_logouts: self.force_logouts,
            }
        } else {
            SessionState::CoolingDown { waited_secs: 0 }
        };
    }

    /// How many force-logouts this session has fired.
    #[must_use]
    pub const fn force_logouts(&self) -> u32 {
        self.force_logouts
    }

    /// Drops the connection when the single-session lock is lost.
    ///
    /// `set_lock_held(false)` alone only flips the flag — the state stays
    /// `Connected`, so `may_connect` reports `AlreadyConnected` and a
    /// caller polling for a drop signal never gets one. We would keep
    /// streaming WITHOUT the lock while a peer takes it: two processes on
    /// a one-login key. This forces the disconnect that the flag implies.
    pub const fn on_lock_lost(&mut self) {
        self.lock_held = false;
        match self.state {
            SessionState::CoolingDown { .. } | SessionState::ParkedGhost { .. } => {}
            _ => self.state = SessionState::Idle,
        }
    }

    /// Handles a vendor auth refusal.
    ///
    /// A `User Already Connected` refusal means a ghost session holds the
    /// single login. The correct response is force-logout + cooldown —
    /// NOT another immediate reconnect. Any other refusal is an ordinary
    /// failure and takes the backoff ladder.
    ///
    /// Returns `true` when the caller should fire the force-logout URL.
    pub fn on_auth_refused(&mut self, message: &str) -> bool {
        // Case-INSENSITIVE, and matched on the distinctive middle of the
        // phrase. The exact wire text is UNVERIFIED-LIVE; if casing or
        // surrounding wording drifts, an exact match would route the ghost
        // to the ordinary backoff ladder, never force-logout, and the
        // ghost would hold the login silently for the whole session.
        if contains_ignore_ascii_case(message, TD_MSG_ALREADY_CONNECTED_CORE) {
            self.on_force_logout_fired();
            true
        } else {
            self.on_disconnected();
            false
        }
    }
}

/// Backoff step for a given consecutive-failure count: 5s doubling to a
/// 60s ceiling.
#[must_use]
pub const fn backoff_step_secs(attempt: u32) -> u64 {
    let min = TD_RECONNECT_BACKOFF_MIN.as_secs();
    let max = TD_RECONNECT_BACKOFF_MAX.as_secs();
    // Shift is bounded so the doubling cannot overflow.
    let shift = if attempt > 5 { 5 } else { attempt };
    let step = min.saturating_mul(1u64 << shift);
    if step > max { max } else { step }
}

impl fmt::Display for ConnectRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CoolingDown { remaining_secs } => {
                write!(f, "force-logout cooldown, {remaining_secs}s remaining")
            }
            Self::Backoff { remaining_secs } => {
                write!(f, "reconnect backoff, {remaining_secs}s remaining")
            }
            Self::LockNotHeld => write!(f, "single-session lock not held"),
            Self::AlreadyConnected => write!(f, "already connected"),
            Self::GhostBudgetExhausted => write!(
                f,
                "force-logout budget spent ({TD_MAX_FORCE_LOGOUTS_PER_SESSION}) — another process holds this API key"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn connected_session() -> TruedataSession {
        let mut s = TruedataSession::new();
        s.set_lock_held(true);
        s.on_connected();
        s
    }

    // --- connect URL / secret handling ---

    #[test]
    fn test_truedata_connect_url_new_builds_documented_shape() {
        let u = TruedataConnectUrl::new("push.truedata.in", TD_PORT_PRODUCTION, "USER1", "PASS1");
        assert_eq!(
            u.expose_secret_url(),
            "wss://push.truedata.in:8084?user=USER1&password=PASS1"
        );
        assert_eq!(u.host(), "push.truedata.in");
        assert_eq!(u.port(), 8084);
    }

    #[test]
    fn test_truedata_connect_url_redacted_hides_credentials() {
        let u =
            TruedataConnectUrl::new("push.truedata.in", TD_PORT_SANDBOX, "SECRETUSER", "hunter2");
        let r = u.redacted();
        assert!(!r.contains("SECRETUSER"), "user must not appear: {r}");
        assert!(!r.contains("hunter2"), "password must not appear: {r}");
        assert!(r.contains("push.truedata.in"));
        assert!(r.contains("8086"));
    }

    #[test]
    fn test_truedata_connect_url_new_escapes_query_breaking_chars() {
        // A password containing '&' would otherwise truncate the URL and
        // produce a baffling auth failure.
        let u = TruedataConnectUrl::new("h", 1, "a&b", "p=q&r");
        let raw = u.expose_secret_url();
        assert!(raw.contains("user=a%26b"));
        assert!(raw.contains("password=p%3Dq%26r")); // secret-scan-ignore: synthetic test value
    }

    #[test]
    fn test_truedata_connect_url_ports_are_the_documented_values() {
        assert_eq!(TD_PORT_SANDBOX, 8086, "v2.6 p.9 sandbox real-time port");
        assert_eq!(
            TD_PORT_PRODUCTION, 8084,
            "v2.6 p.9 production real-time port"
        );
    }

    #[test]
    fn test_itoa_u16_formats_without_allocating() {
        assert_eq!(itoa_u16(0).as_str(), "0");
        assert_eq!(itoa_u16(8084).as_str(), "8084");
        assert_eq!(itoa_u16(65535).as_str(), "65535");
        assert_eq!(itoa_u16(1).as_str(), "1");
    }

    // --- the CRITICAL race: cooldown vs backoff ---

    #[test]
    fn test_may_connect_is_refused_during_force_logout_cooldown() {
        let mut s = connected_session();
        s.on_force_logout_fired();
        assert!(matches!(
            s.may_connect(),
            Err(ConnectRefusal::CoolingDown { .. })
        ));
    }

    #[test]
    fn test_on_force_logout_fired_supersedes_a_shorter_backoff() {
        // THE BUG THIS MODULE EXISTS TO PREVENT: a 5s backoff must never
        // fire inside the vendor's 60s post-force-logout wait.
        let mut s = connected_session();
        s.on_disconnected(); // schedules a 5s backoff
        s.on_force_logout_fired(); // must override it
        // Advance past the backoff step but not past the cooldown.
        s.tick(10);
        match s.may_connect() {
            Err(ConnectRefusal::CoolingDown { remaining_secs }) => {
                assert_eq!(remaining_secs, 50, "60s cooldown, 10s elapsed");
            }
            other => panic!("expected CoolingDown refusal, got {other:?}"),
        }
    }

    #[test]
    fn test_tick_releases_the_session_only_after_the_full_cooldown() {
        let mut s = connected_session();
        s.on_force_logout_fired();
        s.tick(59);
        assert!(
            matches!(s.may_connect(), Err(ConnectRefusal::CoolingDown { .. })),
            "must still refuse one second early"
        );
        s.tick(1);
        assert_eq!(s.state(), SessionState::Idle, "60s elapsed => Idle");
        assert!(s.may_connect().is_ok());
    }

    #[test]
    fn test_cooldown_constant_is_one_minute_per_the_pdf() {
        // v2.6 p.19 verbatim: "Wait for 1 min after firing the URL".
        assert_eq!(TD_FORCE_LOGOUT_COOLDOWN.as_secs(), 60);
    }

    // --- auth refusal routing ---

    #[test]
    fn test_on_auth_refused_already_connected_triggers_force_logout() {
        let mut s = connected_session();
        let should_force = s.on_auth_refused("Access Denied. User Already Connected.");
        assert!(should_force, "duplicate login must trigger force-logout");
        assert!(matches!(s.state(), SessionState::CoolingDown { .. }));
    }

    #[test]
    fn test_on_auth_refused_other_message_takes_the_backoff_ladder() {
        let mut s = connected_session();
        let should_force = s.on_auth_refused("invalid login credentials");
        assert!(!should_force, "a credential error must NOT force-logout");
        assert!(matches!(s.state(), SessionState::Backoff { .. }));
    }

    #[test]
    fn test_already_connected_message_matches_the_pdf_literal() {
        assert_eq!(TD_MSG_ALREADY_CONNECTED, "User Already Connected");
    }

    // --- the reconnect-storm class: the cooldown outranks everything ---

    #[test]
    fn test_on_disconnected_does_not_clobber_the_cooldown() {
        // THE natural ordering, and the one that used to break: the vendor
        // refuses, we force-logout and start the mandatory wait, and THEN
        // the socket closes (it must — we were just refused). That close
        // used to overwrite the 60s cooldown with a 5s backoff, so we would
        // log in again 5 seconds into the vendor's wait, be refused, and
        // repeat for the whole session.
        let mut s = TruedataSession::new();
        s.set_lock_held(true);
        assert!(s.on_auth_refused(TD_MSG_ALREADY_CONNECTED));
        s.on_disconnected();
        assert!(
            matches!(s.state(), SessionState::CoolingDown { .. }),
            "socket close must not cancel the vendor's mandatory wait, got {:?}",
            s.state()
        );
        // And it must still be refusing well past the old 5s backoff step.
        s.tick(10);
        assert!(
            matches!(s.may_connect(), Err(ConnectRefusal::CoolingDown { .. })),
            "must still be cooling down 10s in, got {:?}",
            s.may_connect()
        );
    }

    #[test]
    fn test_on_connected_does_not_clobber_the_cooldown() {
        // A stale success callback from a socket already being torn down
        // must not cancel the wait either.
        let mut s = TruedataSession::new();
        s.set_lock_held(true);
        s.on_force_logout_fired();
        s.on_connected();
        assert!(matches!(s.state(), SessionState::CoolingDown { .. }));
    }

    #[test]
    fn test_cooldown_survives_a_full_disconnect_reconnect_cycle() {
        // Belt and braces: hammer every event at it for the whole minute
        // and the machine must refuse every single connect.
        let mut s = TruedataSession::new();
        s.set_lock_held(true);
        s.on_force_logout_fired();
        for elapsed in 0..59 {
            s.on_disconnected();
            s.on_connect_started();
            s.on_connected();
            assert!(
                s.may_connect().is_err(),
                "connect allowed {elapsed}s into the mandatory wait"
            );
            s.tick(1);
        }
    }

    // --- the ghost loop must terminate ---

    #[test]
    fn test_on_force_logout_fired_parks_after_the_budget() {
        let mut s = TruedataSession::new();
        s.set_lock_held(true);
        for n in 1..TD_MAX_FORCE_LOGOUTS_PER_SESSION {
            s.on_force_logout_fired();
            assert!(
                matches!(s.state(), SessionState::CoolingDown { .. }),
                "attempt {n} should still be cooling down, got {:?}",
                s.state()
            );
            assert_eq!(
                s.force_logouts(),
                n,
                "the budget must ACCUMULATE across cycles"
            );
            s.tick(TD_FORCE_LOGOUT_COOLDOWN.as_secs());
        }
        s.on_force_logout_fired();
        assert!(
            matches!(s.state(), SessionState::ParkedGhost { .. }),
            "budget spent — must park rather than cycle forever, got {:?}",
            s.state()
        );
        assert_eq!(s.may_connect(), Err(ConnectRefusal::GhostBudgetExhausted));
    }

    #[test]
    fn test_force_logouts_counter_survives_the_cooldown_expiring() {
        // The count lives on the SESSION, not on the CoolingDown variant.
        // Held in the state it would be discarded when the cooldown expires
        // to Idle, resetting the budget to zero on every cycle — the ghost
        // loop would still be unbounded, just less obviously so.
        let mut s = TruedataSession::new();
        s.set_lock_held(true);
        assert_eq!(s.force_logouts(), 0);
        s.on_force_logout_fired();
        assert_eq!(s.force_logouts(), 1);
        s.tick(TD_FORCE_LOGOUT_COOLDOWN.as_secs());
        assert_eq!(s.state(), SessionState::Idle, "cooldown expired");
        assert_eq!(
            s.force_logouts(),
            1,
            "the budget must survive the cooldown expiring, or it never accumulates"
        );
        s.on_force_logout_fired();
        assert_eq!(s.force_logouts(), 2);
    }

    #[test]
    fn test_parked_ghost_is_terminal_against_every_event() {
        let mut s = TruedataSession::new();
        s.set_lock_held(true);
        for _ in 0..TD_MAX_FORCE_LOGOUTS_PER_SESSION {
            s.on_force_logout_fired();
            s.tick(TD_FORCE_LOGOUT_COOLDOWN.as_secs());
        }
        assert!(matches!(s.state(), SessionState::ParkedGhost { .. }));
        s.on_disconnected();
        s.on_connect_started();
        s.on_connected();
        s.tick(10_000);
        assert!(
            matches!(s.state(), SessionState::ParkedGhost { .. }),
            "parked is terminal — the operator decides, not a retry timer"
        );
    }

    // --- lock loss must actually drop the connection ---

    #[test]
    fn test_on_lock_lost_forces_disconnect_not_just_a_flag_flip() {
        let mut s = TruedataSession::new();
        s.set_lock_held(true);
        s.on_connect_started();
        s.on_connected();
        assert_eq!(s.state(), SessionState::Connected);
        s.on_lock_lost();
        assert_eq!(
            s.state(),
            SessionState::Idle,
            "losing the lock while connected means two processes on one key"
        );
        assert!(!s.lock_held());
        assert_eq!(s.may_connect(), Err(ConnectRefusal::LockNotHeld));
    }

    // --- we must not knock ourselves offline ---

    #[test]
    fn test_on_connect_started_blocks_a_second_concurrent_connect() {
        // Between "go" and "connected" the machine used to sit in Idle, so
        // a retry timer firing under a slow handshake got Ok again and
        // opened a second socket on a one-login key — the vendor kills the
        // first, and we have knocked ourselves offline.
        let mut s = TruedataSession::new();
        s.set_lock_held(true);
        assert_eq!(s.may_connect(), Ok(()));
        s.on_connect_started();
        assert_eq!(
            s.may_connect(),
            Err(ConnectRefusal::AlreadyConnected),
            "a second attempt while one is in flight must be refused"
        );
    }

    // --- the refusal literal is UNVERIFIED-LIVE, so match it loosely ---

    #[test]
    fn test_on_auth_refused_matches_case_and_wording_drift() {
        for msg in [
            "User Already Connected",
            "user already connected",
            "USER ALREADY CONNECTED",
            "Access Denied. Key already in use by other session. Already Connected.",
        ] {
            let mut s = TruedataSession::new();
            s.set_lock_held(true);
            assert!(
                s.on_auth_refused(msg),
                "must force-logout on {msg:?} — an exact-literal match would \
                 send this down the backoff ladder and the ghost would hold \
                 the login all session"
            );
            assert!(matches!(s.state(), SessionState::CoolingDown { .. }));
        }
    }

    #[test]
    fn test_on_auth_refused_unrelated_message_still_takes_the_ladder() {
        let mut s = TruedataSession::new();
        s.set_lock_held(true);
        assert!(!s.on_auth_refused("Invalid credentials"));
        assert!(matches!(s.state(), SessionState::Backoff { .. }));
    }

    #[test]
    fn test_contains_ignore_ascii_case_boundaries() {
        assert!(contains_ignore_ascii_case(
            "Already Connected",
            "already connected"
        ));
        assert!(contains_ignore_ascii_case(
            "xxALREADY CONNECTEDxx",
            "already connected"
        ));
        assert!(!contains_ignore_ascii_case("already", "already connected"));
        assert!(!contains_ignore_ascii_case("", "a"));
        assert!(!contains_ignore_ascii_case("anything", ""));
        // Exactly-equal lengths must still match.
        assert!(contains_ignore_ascii_case("AbC", "aBc"));
    }

    // --- lock discipline (fail-closed) ---

    #[test]
    fn test_may_connect_refuses_when_the_session_lock_is_not_held() {
        let s = TruedataSession::new(); // lock not acquired
        assert_eq!(s.may_connect(), Err(ConnectRefusal::LockNotHeld));
    }

    #[test]
    fn test_set_lock_held_false_blocks_reconnect_after_lock_loss() {
        let mut s = connected_session();
        s.on_disconnected();
        s.tick(100); // backoff elapsed
        s.set_lock_held(false);
        assert_eq!(
            s.may_connect(),
            Err(ConnectRefusal::LockNotHeld),
            "a lock-lost process must never fight its peer for the single login"
        );
    }

    #[test]
    fn test_may_connect_refuses_a_second_connect_while_connected() {
        let s = connected_session();
        assert_eq!(s.may_connect(), Err(ConnectRefusal::AlreadyConnected));
    }

    // --- backoff ladder ---

    #[test]
    fn test_backoff_step_secs_doubles_then_saturates_at_the_ceiling() {
        assert_eq!(backoff_step_secs(0), 5);
        assert_eq!(backoff_step_secs(1), 10);
        assert_eq!(backoff_step_secs(2), 20);
        assert_eq!(backoff_step_secs(3), 40);
        assert_eq!(backoff_step_secs(4), 60, "80 clamps to the 60s ceiling");
        assert_eq!(backoff_step_secs(50), 60, "never exceeds the ceiling");
        assert_eq!(
            backoff_step_secs(u32::MAX),
            60,
            "no overflow at the extreme"
        );
    }

    #[test]
    fn test_on_disconnected_advances_the_attempt_counter() {
        let mut s = connected_session();
        s.on_disconnected();
        s.tick(999);
        s.on_disconnected();
        match s.state() {
            SessionState::Backoff {
                attempt, step_secs, ..
            } => {
                assert_eq!(attempt, 1);
                assert_eq!(step_secs, 10, "second failure waits longer");
            }
            other => panic!("expected Backoff, got {other:?}"),
        }
    }

    #[test]
    fn test_on_connected_clears_the_backoff() {
        let mut s = connected_session();
        s.on_disconnected();
        s.on_connected();
        assert_eq!(s.state(), SessionState::Connected);
    }

    #[test]
    fn test_backoff_refusal_reports_remaining_time() {
        let mut s = connected_session();
        s.on_disconnected();
        s.tick(2);
        match s.may_connect() {
            Err(ConnectRefusal::Backoff { remaining_secs }) => assert_eq!(remaining_secs, 3),
            other => panic!("expected Backoff refusal, got {other:?}"),
        }
    }

    #[test]
    fn test_full_recovery_cycle_never_deadlocks() {
        // Ghost session -> force logout -> cooldown -> reconnect, with a
        // reconnect attempt tried at every single second along the way.
        let mut s = connected_session();
        assert!(s.on_auth_refused(TD_MSG_ALREADY_CONNECTED));
        for _ in 0..60 {
            assert!(
                s.may_connect().is_err(),
                "every attempt inside the cooldown must be refused"
            );
            s.tick(1);
        }
        assert!(
            s.may_connect().is_ok(),
            "the session MUST become connectable once the cooldown expires"
        );
    }

    #[test]
    fn test_connect_refusal_display_is_operator_readable() {
        let d = ConnectRefusal::CoolingDown { remaining_secs: 42 }.to_string();
        assert!(d.contains("42"));
        assert!(d.contains("cooldown"));
        assert!(ConnectRefusal::LockNotHeld.to_string().contains("lock"));
    }

    #[test]
    fn test_host_returns_the_slice_without_a_second_allocation() {
        // host() slices out of the URL rather than storing a copy, so a
        // regression to a stored String would show up as a mismatch here.
        let u = TruedataConnectUrl::new("push.truedata.in", TD_PORT_PRODUCTION, "u", "p");
        assert_eq!(u.host(), "push.truedata.in");
        let short = TruedataConnectUrl::new("a", 1, "u", "p");
        assert_eq!(short.host(), "a");
        let empty = TruedataConnectUrl::new("", 1, "u", "p");
        assert_eq!(
            empty.host(),
            "",
            "an empty host must not panic or over-slice"
        );
    }

    #[test]
    fn test_expose_secret_url_is_the_only_path_to_the_raw_credentials() {
        let u = TruedataConnectUrl::new("h", 8084, "USER", "PASS");
        let raw = u.expose_secret_url();
        assert!(raw.contains("USER"), "the raw URL carries the credentials");
        assert!(raw.starts_with("wss://h:8084?"));
        // ...and the redacted form deliberately does NOT.
        assert!(!u.redacted().contains("USER"));
    }

    #[test]
    fn test_state_reports_each_lifecycle_transition() {
        let mut s = TruedataSession::new();
        assert_eq!(s.state(), SessionState::Idle);
        s.set_lock_held(true);
        s.on_connected();
        assert_eq!(s.state(), SessionState::Connected);
        s.on_disconnected();
        assert!(matches!(s.state(), SessionState::Backoff { .. }));
        s.on_force_logout_fired();
        assert!(matches!(s.state(), SessionState::CoolingDown { .. }));
        assert!(s.lock_held(), "lock state is independent of session state");
    }
}

// ---------------------------------------------------------------------------
// Heartbeat watchdog
// ---------------------------------------------------------------------------

/// Documented heartbeat cadence (v2.6 p.11, verbatim: "every 5-6 seconds
/// interval"). We pin the SLOWER end so the watchdog never fires on the
/// vendor's own documented jitter.
pub const TD_HEARTBEAT_INTERVAL_SECS: u64 = 6;

/// Missed heartbeats before the link is called LATE (degraded, logged).
pub const TD_HEARTBEAT_LATE_MISSES: u64 = 3;
/// Missed heartbeats before the link is called STALLED (reconnect).
pub const TD_HEARTBEAT_STALL_MISSES: u64 = 6;

/// Silence, in seconds, at which the link is LATE.
pub const TD_HEARTBEAT_LATE_SECS: u64 = TD_HEARTBEAT_INTERVAL_SECS * TD_HEARTBEAT_LATE_MISSES;
/// Silence, in seconds, at which the link is STALLED.
pub const TD_HEARTBEAT_STALL_SECS: u64 = TD_HEARTBEAT_INTERVAL_SECS * TD_HEARTBEAT_STALL_MISSES;

const _: () = assert!(TD_HEARTBEAT_LATE_SECS < TD_HEARTBEAT_STALL_SECS);

/// Health of the link as judged by heartbeat silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkHealth {
    /// A heartbeat arrived within the expected window.
    Healthy,
    /// Heartbeats are late but not yet conclusive — log, do not reconnect.
    Late {
        /// Seconds since the last heartbeat.
        silent_secs: u64,
    },
    /// Heartbeats have stopped: the link is dead even if the socket has
    /// not reported an error. Reconnect.
    Stalled {
        /// Seconds since the last heartbeat.
        silent_secs: u64,
    },
}

impl LinkHealth {
    /// `true` only when a reconnect is warranted.
    #[must_use]
    pub const fn should_reconnect(self) -> bool {
        matches!(self, Self::Stalled { .. })
    }
}

/// Detects a dead link from heartbeat silence.
///
/// This is the feed-death signal that a socket error does NOT give you: a
/// TCP connection can stay open while the vendor has stopped sending, and
/// without this the lane would sit "connected" and silent all session.
///
/// Heartbeats are the right signal rather than ticks because v2.6 p.11
/// states they begin "as soon as the client is connected" and continue
/// independently of trading activity — so silence is unambiguous, whereas
/// tick silence is legitimate outside market hours.
#[derive(Debug, Clone, Copy, Default)]
pub struct TruedataHeartbeatWatchdog {
    silent_secs: u64,
}

impl TruedataHeartbeatWatchdog {
    /// A fresh watchdog with no silence recorded.
    #[must_use]
    pub const fn new() -> Self {
        Self { silent_secs: 0 }
    }

    /// Records a heartbeat, clearing the silence counter.
    pub const fn on_heartbeat(&mut self) {
        self.silent_secs = 0;
    }

    /// Advances the silence clock.
    pub const fn tick(&mut self, secs: u64) {
        self.silent_secs = self.silent_secs.saturating_add(secs);
    }

    /// Seconds since the last heartbeat.
    #[must_use]
    pub const fn silent_secs(&self) -> u64 {
        self.silent_secs
    }

    /// Current verdict.
    #[must_use]
    pub const fn health(&self) -> LinkHealth {
        if self.silent_secs >= TD_HEARTBEAT_STALL_SECS {
            LinkHealth::Stalled {
                silent_secs: self.silent_secs,
            }
        } else if self.silent_secs >= TD_HEARTBEAT_LATE_SECS {
            LinkHealth::Late {
                silent_secs: self.silent_secs,
            }
        } else {
            LinkHealth::Healthy
        }
    }
}

#[cfg(test)]
mod watchdog_tests {
    use super::*;

    #[test]
    fn test_truedata_heartbeat_watchdog_new_starts_healthy() {
        let w = TruedataHeartbeatWatchdog::new();
        assert_eq!(w.health(), LinkHealth::Healthy);
        assert_eq!(w.silent_secs(), 0);
        assert!(!w.health().should_reconnect());
    }

    #[test]
    fn test_on_heartbeat_clears_accumulated_silence() {
        let mut w = TruedataHeartbeatWatchdog::new();
        w.tick(20);
        assert_ne!(w.health(), LinkHealth::Healthy);
        w.on_heartbeat();
        assert_eq!(w.health(), LinkHealth::Healthy);
        assert_eq!(w.silent_secs(), 0);
    }

    #[test]
    fn test_health_stays_healthy_across_the_documented_cadence() {
        // v2.6 p.11 says every 5-6 seconds; a normal beat at 6s must NOT
        // register as late, or the watchdog would cry wolf every cycle.
        let mut w = TruedataHeartbeatWatchdog::new();
        for _ in 0..10 {
            w.tick(TD_HEARTBEAT_INTERVAL_SECS);
            assert_eq!(
                w.health(),
                LinkHealth::Healthy,
                "a beat arriving at the documented interval is healthy"
            );
            w.on_heartbeat();
        }
    }

    #[test]
    fn test_health_reports_late_before_stalled() {
        let mut w = TruedataHeartbeatWatchdog::new();
        w.tick(TD_HEARTBEAT_LATE_SECS);
        match w.health() {
            LinkHealth::Late { silent_secs } => assert_eq!(silent_secs, TD_HEARTBEAT_LATE_SECS),
            other => panic!("expected Late, got {other:?}"),
        }
        assert!(!w.health().should_reconnect(), "Late must NOT reconnect");
    }

    #[test]
    fn test_health_reports_stalled_and_requests_reconnect() {
        let mut w = TruedataHeartbeatWatchdog::new();
        w.tick(TD_HEARTBEAT_STALL_SECS);
        match w.health() {
            LinkHealth::Stalled { silent_secs } => {
                assert_eq!(silent_secs, TD_HEARTBEAT_STALL_SECS);
            }
            other => panic!("expected Stalled, got {other:?}"),
        }
        assert!(w.health().should_reconnect());
    }

    #[test]
    fn test_health_boundary_is_exact_at_each_threshold() {
        let mut w = TruedataHeartbeatWatchdog::new();
        w.tick(TD_HEARTBEAT_LATE_SECS.saturating_sub(1));
        assert_eq!(w.health(), LinkHealth::Healthy, "one second before Late");
        w.tick(1);
        assert!(matches!(w.health(), LinkHealth::Late { .. }));

        let mut w2 = TruedataHeartbeatWatchdog::new();
        w2.tick(TD_HEARTBEAT_STALL_SECS.saturating_sub(1));
        assert!(
            matches!(w2.health(), LinkHealth::Late { .. }),
            "one second before Stalled is still only Late"
        );
        w2.tick(1);
        assert!(matches!(w2.health(), LinkHealth::Stalled { .. }));
    }

    #[test]
    fn test_tick_saturates_without_overflow() {
        let mut w = TruedataHeartbeatWatchdog::new();
        w.tick(u64::MAX);
        w.tick(u64::MAX);
        assert!(w.health().should_reconnect());
        assert_eq!(w.silent_secs(), u64::MAX, "saturating, never wrapping");
    }

    #[test]
    fn test_heartbeat_thresholds_are_ordered_and_documented() {
        assert_eq!(TD_HEARTBEAT_INTERVAL_SECS, 6, "v2.6 p.11 slow end of 5-6s");
        assert_eq!(TD_HEARTBEAT_LATE_SECS, 18);
        assert_eq!(TD_HEARTBEAT_STALL_SECS, 36);
        assert!(TD_HEARTBEAT_LATE_SECS < TD_HEARTBEAT_STALL_SECS);
    }

    #[test]
    fn test_link_health_should_reconnect_only_for_stalled() {
        assert!(!LinkHealth::Healthy.should_reconnect());
        assert!(!LinkHealth::Late { silent_secs: 20 }.should_reconnect());
        assert!(LinkHealth::Stalled { silent_secs: 40 }.should_reconnect());
    }
}
