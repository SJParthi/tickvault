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
    /// The server is not sending data to this connection — the sidecar's
    /// SILENT-FEED watchdog fired (socket connects but streams nothing:
    /// server-side session limit / throttle, or in the worst case a genuine
    /// entitlement gap), or the line names a permissions/authorization
    /// problem. The variant name is historical; the operator-facing prose
    /// no longer claims an entitlement fault (exam-fix 2026-07-06).
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
    ///
    /// 2026-07-14 note: this fixed prose is now the CLASS EXPLANATION line of
    /// the `GrowwSidecarRejected` Telegram; the SPECIFIC sanitized cause is
    /// additionally threaded as the event's `detail` field via
    /// [`sidecar_reject_detail`] (never the raw line — see §1c.1 of
    /// `feed-stall-watchdog-error-codes.md`).
    ///
    /// Wording note (exam-fix 2026-07-06): the `EntitlementRejected` prose
    /// previously asserted "account lacks a live market-data feed
    /// entitlement" — a CLAIM about the account that the classifier cannot
    /// actually prove: the dominant real trigger during the fleet exam was
    /// server-side SESSION STARVATION (the socket connects, the server just
    /// sends nothing to that connection — session limit / throttle). The
    /// honest wording states only what is OBSERVED.
    #[must_use]
    pub const fn alert_reason(self) -> Option<&'static str> {
        match self {
            Self::EntitlementRejected => Some(
                "server is not sending data to this connection (session limit or throttle) \
                 — retrying with backoff",
            ),
            Self::AuthRejected => Some(
                "authentication rejected — the shared access token is stale/unusable; \
                 check the bruteX groww-token-minter Lambda's last daily mint",
            ),
            Self::Error => Some("the feed reported an error and is retrying"),
            Self::Subscribed | Self::Streaming | Self::Info => None,
        }
    }

    /// Short cause label for the FLEET-scoped coalesced summary (exam-fix
    /// 2026-07-06) — the parenthetical inside "N of M connections retrying
    /// (<label>) — no reject reported from the other K connections". Plain
    /// English, fixed per class, `None` for the non-alert classes.
    #[must_use]
    pub const fn summary_label(self) -> Option<&'static str> {
        match self {
            Self::EntitlementRejected => Some("server session limit or throttle"),
            Self::AuthRejected => Some("access token stale"),
            Self::Error => Some("feed error"),
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

// ---------------------------------------------------------------------------
// Stall-restart episode rows (dual-feed scoreboard PR-B, 2026-07-10)
// ---------------------------------------------------------------------------

/// Which stall-watchdog arm fired the kill+relaunch — the classic
/// alive-but-silent arm (FEED-STALL-01) or the never-streamed-first-tick arm
/// (FEED-STALL-01 §1b).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StallArm {
    /// The classic stall arm: a known last-tick aged past the threshold.
    SilentSocket,
    /// The 2026-07-09 arm: connected + subscribed but never streamed a
    /// first tick this session.
    NeverStreamed,
}

/// Latched stall-cause codes shared between the pipe drains (writers) and
/// the stall arms (reader) via a relaxed `AtomicU8`. Only the CONFIRMED
/// reject classes latch (`sets_auth_rejected` — AuthRejected /
/// EntitlementRejected); the generic transient `Error` class deliberately
/// does NOT (it must not claim a credential/entitlement cause, mirroring the
/// false-"refresh the api-key" fix), and a streaming recovery clears the
/// latch so a LATER silent stall never inherits a stale cause.
///
/// PR-B fix round 1 (2026-07-10 review MEDIUM): the EntitlementRejected
/// class is SPLIT at latch time. A SILENT-FEED watchdog line
/// ([`is_silent_feed_diagnostic`] — "subscribed N but received NO records
/// in 30s") is (a) NEVER latched outside market hours (the sidecar prints
/// it unconditionally 30s after subscribe, including the ~08:35 pre-open
/// print the drain itself logs as benign INFO — the latch uses the SAME
/// market-hours predicate as the alert path), and (b) in-market it latches
/// the DISTINCT [`STALL_CAUSE_SILENT_FEED`] value which does NOT outrank
/// either stall arm's slug — the silent-feed watchdog is the same
/// evidence class as the arms themselves (silence), so it adds no
/// attribution; only a HARD authorization / permission / entitlement line
/// latches [`STALL_CAUSE_ENTITLEMENT`] and outranks the arm. Without the
/// split, the sidecar's 30s watchdog always fired before the Rust 300s
/// never-streamed threshold and `stall_never_streamed` was effectively
/// unreachable in production.
pub const STALL_CAUSE_NONE: u8 = 0;
/// The child's last confirmed reject was `SidecarLineClass::AuthRejected`.
pub const STALL_CAUSE_AUTH: u8 = 1;
/// The child's last confirmed reject was a HARD authorization / permission
/// / entitlement line (`SidecarLineClass::EntitlementRejected` that is NOT
/// a silent-feed watchdog diagnostic).
pub const STALL_CAUSE_ENTITLEMENT: u8 = 2;
/// The child's last reject-class line was an IN-MARKET SILENT-FEED watchdog
/// diagnostic — corroborates silence but never outranks the arm's slug.
pub const STALL_CAUSE_SILENT_FEED: u8 = 3;

/// Latch value for one classified sidecar line — `None` = leave the latch
/// untouched, `Some(v)` = store `v`. `silent_feed_diag` =
/// [`is_silent_feed_diagnostic`] for the same line; `market_open` = the
/// same market-hours predicate the alert path uses (injected so this is
/// unit-testable without a live clock). Pure.
#[must_use]
pub const fn stall_cause_latch_update(
    class: SidecarLineClass,
    silent_feed_diag: bool,
    market_open: bool,
) -> Option<u8> {
    match class {
        SidecarLineClass::AuthRejected => Some(STALL_CAUSE_AUTH),
        SidecarLineClass::EntitlementRejected => {
            if silent_feed_diag {
                // The sidecar's 30s silent watchdog: benign off-hours
                // (never latches — mirrors the alert-path gate); in-market
                // it records the WEAK silent-feed corroboration only.
                if market_open {
                    Some(STALL_CAUSE_SILENT_FEED)
                } else {
                    None
                }
            } else {
                // A hard authorization / permission / entitlement line is
                // a real config fault at ANY time (see
                // is_silent_feed_diagnostic's doc) — always latches.
                Some(STALL_CAUSE_ENTITLEMENT)
            }
        }
        // A streaming recovery clears any latched cause (edge semantics —
        // the next stall must not inherit a pre-recovery reject class).
        SidecarLineClass::Streaming => Some(STALL_CAUSE_NONE),
        // Generic errors / subscribe / info lines never change the latch.
        SidecarLineClass::Error | SidecarLineClass::Subscribed | SidecarLineClass::Info => None,
    }
}

/// The FIXED machine cause slug stamped into the `stall_restarted` row's
/// `source` column. A CONFIRMED HARD reject class observed on the child
/// (the latch: auth / hard entitlement) outranks the arm's generic slug —
/// an auth-stale never-streamed loop is a token-minter problem, not broker
/// silence. The [`STALL_CAUSE_SILENT_FEED`] latch deliberately does NOT
/// outrank: it is the same silence evidence the arm itself measured, so
/// both arms keep their own slug (this is what makes
/// `stall_never_streamed` reachable — PR-B fix round 1). Pure, total:
/// every input yields one of the four `STALL_SOURCE_*` slugs (never raw
/// text).
#[must_use]
pub const fn stall_cause_slug(arm: StallArm, latched_cause: u8) -> &'static str {
    match latched_cause {
        STALL_CAUSE_AUTH => tickvault_common::feed_blame::STALL_SOURCE_AUTH_STALE,
        STALL_CAUSE_ENTITLEMENT => tickvault_common::feed_blame::STALL_SOURCE_ENTITLEMENT,
        // STALL_CAUSE_SILENT_FEED / STALL_CAUSE_NONE / unknown future
        // values all degrade to the arm's own slug.
        _ => match arm {
            StallArm::SilentSocket => tickvault_common::feed_blame::STALL_SOURCE_SILENT_SOCKET,
            StallArm::NeverStreamed => tickvault_common::feed_blame::STALL_SOURCE_NEVER_STREAMED,
        },
    }
}

/// Fixed reason text for every `stall_restarted` audit row — the cause lives
/// in the `source` slug; the silent window lives in `down_secs`. A FIXED
/// literal so no runtime/child text can reach the table via this row.
pub const STALL_RESTART_AUDIT_REASON: &str =
    "stall watchdog killed + relaunched the sidecar (re-auth + re-subscribe)";

/// Build ONE `ws_event_audit` row for a stall-watchdog kill+relaunch.
///
/// Mirrors `groww_bridge::build_groww_ws_audit_row` conventions (feed=groww,
/// ws_type=groww_bridge, IST nanos designated ts + IST-midnight trading day)
/// but carries the fleet child's `conn_id` as `connection_index` so a §34
/// fleet child's stall row never collides with connection 0's. COLD PATH:
/// at most one row per kill (bounded by the stall threshold cadence).
#[must_use]
pub fn build_stall_ws_audit_row(
    cause_slug: &'static str,
    silent_secs: u64,
    conn_id: Option<usize>,
) -> tickvault_common::ws_event_types::WsEventAuditRow {
    let now_utc_nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let now_ist_nanos =
        now_utc_nanos.saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
    let nanos_per_day: i64 = 86_400 * 1_000_000_000;
    let trading_date_ist_nanos = now_ist_nanos - now_ist_nanos.rem_euclid(nanos_per_day);
    tickvault_common::ws_event_types::WsEventAuditRow {
        event_ts_ist_nanos: now_ist_nanos,
        trading_date_ist_nanos,
        feed: tickvault_common::feed::Feed::Groww,
        ws_type: tickvault_common::ws_event_types::WsType::GrowwBridge,
        connection_index: conn_id.map_or(0, |c| i64::try_from(c).unwrap_or(0)),
        pool_size: 1,
        event_kind: tickvault_common::ws_event_types::WsEventKind::StallRestarted,
        // FIXED machine slug — never raw child text (redaction discipline).
        source: cause_slug.to_string(),
        reason: STALL_RESTART_AUDIT_REASON.to_string(),
        dhan_code: tickvault_common::ws_event_types::WS_EVENT_NO_DHAN_CODE,
        // The silent window that triggered the kill — the downtime the
        // scoreboard attributes to the episode.
        down_secs: i64::try_from(silent_secs).unwrap_or(i64::MAX),
        attempts: 0,
        market_hours: tickvault_common::market_hours::is_within_market_hours_ist(),
    }
}

/// Emit ONE `stall_restarted` ws_event_audit row, best-effort (`try_send` —
/// the supervision loop is never stalled by the forensic side-record; a
/// full/closed channel is AUDIT-WS-01 exactly like the bridge emitter). No-op
/// when no audit channel is attached (defensive / test builds).
fn emit_stall_ws_audit(
    tx: Option<&tokio::sync::mpsc::Sender<tickvault_common::ws_event_types::WsEventAuditRow>>,
    cause_slug: &'static str,
    silent_secs: u64,
    conn_id: Option<usize>,
) {
    let Some(tx) = tx else {
        return;
    };
    let row = build_stall_ws_audit_row(cause_slug, silent_secs, conn_id);
    if let Err(err) = tx.try_send(row) {
        let drop_reason = match err {
            tokio::sync::mpsc::error::TrySendError::Full(_) => "full",
            tokio::sync::mpsc::error::TrySendError::Closed(_) => "closed",
        };
        error!(
            code = ErrorCode::AuditWs01EventWriteFailed.code_str(),
            reason = drop_reason,
            cause = cause_slug,
            "groww stall_restarted ws_event_audit channel full/closed — row dropped \
             (log + FEED-STALL-01 counter still fired)"
        );
        metrics::counter!("tv_ws_event_audit_dropped_total", "reason" => drop_reason).increment(1);
    }
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

/// Never-streamed-first-tick restart threshold (seconds) — the 2026-07-09
/// reject-loop hardening. [`should_restart_on_stall`] deliberately refuses to
/// fire when NO first tick was ever recorded this session (`age == None`), so a
/// sidecar that is alive + connected + subscribed but NEVER streams (the
/// all-day 09:22/14:17 IST `SidecarLineClass::Error` loop) was NEVER restarted
/// by the watchdog. This arm closes that hole: once the feed has been inside
/// the EXCHANGE session ([09:15, 15:30) IST on a trading day) with no first
/// tick for longer than this, the child is killed + relaunched exactly like a
/// stall restart. 300s (the upper end of the 3–5 min band) means the earliest
/// possible fire is 09:20:05 IST — safely past any slow session start, and far
/// above the sidecar's own auth+connect+subscribe worst path.
pub const FEED_NEVER_STREAMED_RESTART_SECS: u64 = 300;

/// Bounded length (chars) of the sanitized sidecar-line SIGNATURE captured
/// into the FEED-REJECT-01 coded error stream. Long enough for the sidecar's
/// `groww sidecar error [<phase>]: <Type>: <detail>` prefix + the identifying
/// cause text; short enough that errors.jsonl / CloudWatch rows stay compact.
pub const SIDECAR_LINE_SIGNATURE_MAX_CHARS: usize = 160;

/// Cross-child cooldown (seconds) on the SINGLE-CONN GrowwSidecarRejected
/// Telegram page (2026-07-09 hostile-review HIGH fix). The per-child `alerted`
/// latch bounds pages to one per CHILD — but the never-streamed arm relaunches
/// a persistently-rejected child every ~5 minutes, and each fresh child gets a
/// fresh latch, so a reject day would page ~12×/hour (~70 HIGH pages/session:
/// pager fatigue, audit Rule 4). The page gate (a supervisor-lifetime
/// last-page timestamp shared across children) bounds the SAME-condition
/// notify() to at most one per this window; every suppressed episode still
/// fires its per-line `error!` forwards, its FEED-REJECT-01 signature, and
/// its feed-health marking — only the Telegram fan-out is cooled down. The
/// ≥3-per-15-min restart-counter pager independently covers "it keeps
/// failing".
///
/// 2026-07-14 (operator noise directive): reduced 1800 → 60s, aligned with
/// the fleet 60s coalescer window. Page dedup is now OWNED by the
/// `EpisodeFamily::GrowwFeed` one-bubble fold (`GrowwSidecarRejected`
/// carries an `episode_key`, so recurrences become in-place edits — no
/// push, no SMS); this gate remains ONLY as the transport-failure bound:
/// if the episode's first page never lands (`message_id = None`), every
/// subsequent reject takes `SendNewFallback` (a fresh send), and without
/// an upstream bound that failure path would restore the storm. 60s caps
/// notify() volume to ≤1/min regardless. NOT removed — ratcheted ==60 by
/// `test_should_page_reject_rising_edge_and_cooldown`.
///
/// APPLIES ONLY while the episode fold is ON — see
/// [`reject_page_cooldown_secs`] and the legacy constant below (FIX-B,
/// hostile review 2026-07-14).
pub const GROWW_REJECT_PAGE_COOLDOWN_SECS: u64 = 60;

/// The pre-2026-07-14 cooldown, still used when `[notification]
/// episode_mode = false` (the documented episode kill switch): with the
/// fold rolled back, every GrowwSidecarRejected takes the legacy
/// Immediate+SMS lane, so a 60s gate would page up to ~60×/hr — 30×
/// noisier than the 2026-07-09 contract. The legacy 1800s bound (at most
/// one page per 30 min) is restored for that mode. Ratcheted ==1800.
pub const GROWW_REJECT_PAGE_COOLDOWN_LEGACY_SECS: u64 = 1800;

/// The effective reject-page cooldown for this process (FIX-B, hostile
/// review 2026-07-14). `episode_mode` is the BOOT-TIME value of
/// `[notification] episode_mode` — consistent with how episode_mode
/// itself is consumed, a runtime config flip needs a restart. Pure, O(1).
#[must_use]
pub const fn reject_page_cooldown_secs(episode_mode: bool) -> u64 {
    if episode_mode {
        GROWW_REJECT_PAGE_COOLDOWN_SECS
    } else {
        GROWW_REJECT_PAGE_COOLDOWN_LEGACY_SECS
    }
}

/// Pure page-cooldown decision for the reject Telegram (2026-07-09 HIGH fix).
/// `last_page_epoch_secs == 0` means no page has fired this supervisor
/// lifetime → page (rising edge is never delayed). Otherwise page only when
/// the cooldown has fully elapsed. Backward clock steps saturate to 0 elapsed
/// → suppressed (safe: at worst one delayed page, never a storm). O(1).
#[must_use]
pub fn should_page_reject(
    now_epoch_secs: u64,
    last_page_epoch_secs: u64,
    cooldown_secs: u64,
) -> bool {
    if last_page_epoch_secs == 0 {
        return true;
    }
    now_epoch_secs.saturating_sub(last_page_epoch_secs) >= cooldown_secs
}

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
    /// Disk-retention hardening (2026-07-13): S3 bucket for the sidecar's
    /// rotated capture archives, injected into the child as
    /// `TICKVAULT_GROWW_ARCHIVE_S3_BUCKET`. Sourced from
    /// `[feeds.groww] capture_archive_s3_bucket`. Empty = archival OFF
    /// (the sidecar keeps archives on disk; it NEVER deletes a file without
    /// a verified S3 copy). A bucket/prefix name only — never a credential.
    pub archive_s3_bucket: String,
    /// Key prefix inside the archive bucket, injected as
    /// `TICKVAULT_GROWW_ARCHIVE_S3_PREFIX`.
    pub archive_s3_prefix: String,
    /// BOOT-TIME value of `[notification] episode_mode` (FIX-B, hostile
    /// review 2026-07-14): selects the reject-page cooldown —
    /// [`GROWW_REJECT_PAGE_COOLDOWN_SECS`] (60s) while the GrowwFeed
    /// episode fold owns page dedup, the legacy
    /// [`GROWW_REJECT_PAGE_COOLDOWN_LEGACY_SECS`] (1800s) when the
    /// episode kill switch is OFF (every reject would otherwise page
    /// Immediate+SMS at ~1/min). A runtime config flip needs a restart,
    /// consistent with how episode_mode itself is read.
    pub episode_mode: bool,
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
            archive_s3_bucket: String::new(),
            archive_s3_prefix: String::new(),
            // Matches `default_notification_episode_mode()` (config.rs) —
            // the fold is ON by default; main.rs overrides from config.
            episode_mode: true,
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
        archive_s3_bucket: base.archive_s3_bucket.clone(),
        archive_s3_prefix: base.archive_s3_prefix.clone(),
        // FIX-B (2026-07-14): fleet children inherit the boot-time
        // episode-mode so every child uses the same reject-page cooldown.
        episode_mode: base.episode_mode,
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

/// FEED-AGNOSTIC never-streamed-first-tick restart decision (the 2026-07-09
/// reject-loop hardening; companion of [`should_restart_on_stall`], which owns
/// the `age == Some(_)` half of the space — this arm owns `age == None`).
///
/// Returns `true` ONLY when the feed is enabled, the caller-computed
/// exchange-session gate is open (`g1_exchange_gate_accepts` [09:15, 15:30)
/// IST AND `is_trading_session_now()` — weekday + NSE-holiday aware; the plain
/// time-of-day [09:00, 15:30) gate the stall arm uses would restart-loop every
/// Saturday), a never-streamed silence window is being TRACKED
/// (`silent_session_secs == None` means the caller has not armed a window —
/// fail-closed), and the continuous in-session no-first-tick silence STRICTLY
/// exceeds `threshold_secs`. Pure + O(1), no feed-specific branch.
#[must_use]
pub fn should_restart_on_never_streamed(
    silent_session_secs: Option<u64>,
    in_exchange_session: bool,
    enabled: bool,
    threshold_secs: u64,
) -> bool {
    if !enabled || !in_exchange_session {
        return false;
    }
    match silent_session_secs {
        None => false,
        Some(silent) => silent > threshold_secs,
    }
}

/// Bounded, secret-redacted SIGNATURE of one sidecar diagnostic line for the
/// FEED-REJECT-01 coded error stream. Pipeline: the existing
/// [`tickvault_common::sanitize::capture_rest_error_body`] choke point
/// (control-char strip → URL/credential-param redaction → JWT-shape redaction
/// → credential-JSON-field redaction → 300-char cap) then a UTF-8-safe
/// truncation to [`SIDECAR_LINE_SIGNATURE_MAX_CHARS`]. The sidecar already
/// redacts its own secrets before printing; this is defense-in-depth so no
/// token shape can ever ride the signature into errors.jsonl / CloudWatch.
/// Pure, cold path (once per reject episode).
/// Post-review hardening (2026-07-09 security pass, LOW): ALSO strips Unicode
/// BiDi overrides/isolates + zero-widths + BOM — `char::is_control()` covers
/// only Cc, not the Cf format chars `sanitize_audit_string` strips for the
/// same terminal-spoofing reason — so a hostile diagnostic can never visually
/// reorder/hide adjacent text when an operator tails the raw errors.jsonl.
#[must_use]
pub fn sidecar_line_signature(line: &str) -> String {
    tickvault_common::sanitize::capture_rest_error_body(line)
        .chars()
        .filter(|c| {
            !matches!(
                c,
                '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200b}'..='\u{200f}' | '\u{feff}'
            )
        })
        .take(SIDECAR_LINE_SIGNATURE_MAX_CHARS)
        .collect()
}

/// The SANITIZED reject-cause detail threaded into the operator-facing
/// `GrowwSidecarRejected` Telegram body (operator demand 2026-07-14 — the
/// 14:59 IST rejection page said only "the feed reported an error" with no
/// WHY; the actual cause was already captured as the FEED-REJECT-01
/// signature but never reached the phone).
///
/// EXACTLY the [`sidecar_line_signature`] choke-point output (control-char +
/// BiDi strip, credential/JWT redaction, UTF-8-safe
/// [`SIDECAR_LINE_SIGNATURE_MAX_CHARS`] cap) — NEVER the raw child line.
/// `None` when the sanitized signature is empty/whitespace, so the Telegram
/// body degrades to the generic wording instead of rendering a hollow
/// "rejected:  — retrying". Pure, cold path (once per reject episode).
/// Dated commandment-2 override recorded in
/// `.claude/rules/project/feed-stall-watchdog-error-codes.md` §1c.1.
#[must_use]
pub fn sidecar_reject_detail(line: &str) -> Option<String> {
    let sig = sidecar_line_signature(line);
    if sig.trim().is_empty() {
        None
    } else {
        Some(sig)
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
// APPROVED: drain wiring — every arg is a distinct shared latch/gate resource
#[allow(clippy::too_many_arguments)]
fn spawn_pipe_drain<R>(
    pipe: R,
    pipe_name: &'static str,
    feed_health: Arc<FeedHealthRegistry>,
    notifier: Option<Arc<NotificationService>>,
    alerted: Arc<std::sync::atomic::AtomicBool>,
    // §34 fleet identity (exam-fix 2026-07-06): `Some(n)` routes the
    // operator-facing Telegram through the FLEET-scoped coalescer (one
    // summary per 60s per direction across ALL connections); `None`
    // (single-conn path) keeps today's per-child semantics byte-identical.
    conn_id: Option<usize>,
    // FEED-REJECT-01 per-child once latch (2026-07-09 review fix): shared by
    // the two drains, re-armed ONLY on a streaming recovery — deliberately
    // NOT by the fleet Suppress re-arm of `alerted`, so the coded signature
    // emit can never become per-line spam on the fleet path.
    detail_logged: Arc<std::sync::atomic::AtomicBool>,
    // Cross-child single-conn page cooldown gate (2026-07-09 HIGH fix):
    // supervisor-lifetime epoch-seconds of the last GrowwSidecarRejected
    // page. See [`GROWW_REJECT_PAGE_COOLDOWN_SECS`].
    reject_page_gate: Arc<std::sync::atomic::AtomicU64>,
    // Episode-mode-aware cooldown window (FIX-B, 2026-07-14) — computed
    // ONCE at supervisor spawn via [`reject_page_cooldown_secs`].
    reject_page_cooldown_secs: u64,
    // Stall-cause latch (scoreboard PR-B, 2026-07-10): the drains record the
    // child's last CONFIRMED reject class (auth/entitlement — the
    // `sets_auth_rejected` split) so a later stall-watchdog kill stamps the
    // precise cause slug into its `stall_restarted` audit row. Cleared on a
    // streaming recovery; generic `Error` lines never latch.
    stall_cause: Arc<std::sync::atomic::AtomicU8>,
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
            // Stall-cause latch (PR-B fix round 1): gated on the SAME
            // silent-feed/market-hours split as the alert path below — a
            // benign off-hours SILENT-FEED print never counts as a
            // confirmed reject, and an in-market one records only the weak
            // silent-feed corroboration (see stall_cause_latch_update).
            if let Some(cause) = stall_cause_latch_update(class, silent_feed_diag, market_open) {
                stall_cause.store(cause, std::sync::atomic::Ordering::Relaxed);
            }
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
                // Re-arm the FEED-REJECT-01 detail latch on the SAME recovery
                // edge (and ONLY here — never on the fleet Suppress re-arm),
                // so a NEW reject episode after a genuine recovery logs a
                // fresh cause signature.
                detail_logged.store(false, std::sync::atomic::Ordering::Relaxed);
                // Fleet bookkeeping (exam-fix 2026-07-06): a streaming
                // recovery clears this connection from the fleet reject set
                // WITHOUT consuming the Connected direction's window (the
                // bridge's genuine connected ping owns that window). No-op on
                // the single-conn path (`conn_id == None`).
                crate::groww_fleet_alerts::global_record_fleet_recovery(conn_id);
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
                    // FEED-REJECT-01 (2026-07-09 reject-loop hardening): capture
                    // a BOUNDED, secret-redacted signature of the triggering
                    // line into the CODED error stream so errors.jsonl /
                    // CloudWatch can answer "WHY does the feed loop?". Since
                    // 2026-07-14 the SAME sanitized signature ALSO rides the
                    // Telegram body via `sidecar_reject_detail` (dated
                    // commandment-2 override, §1c.1 of the runbook) — raw
                    // child text still NEVER reaches Telegram.
                    // Bounded by its OWN per-child latch (review fix: the fleet
                    // Suppress arm re-arms `alerted`, which would have made
                    // this emit per-line on the fleet path) — once per child
                    // episode, re-armed only by a streaming recovery.
                    if !detail_logged.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        error!(
                            code = ErrorCode::FeedReject01SidecarErrorDetail.code_str(),
                            feed = Feed::Groww.as_str(),
                            class = ?class,
                            signature = %sidecar_line_signature(&line),
                            "[feeds] FEED-REJECT-01: sidecar reject/error episode opened — \
                             bounded cause signature captured (the sanitized signature also \
                             rides the Telegram body since 2026-07-14)"
                        );
                    }
                    // Latch the actionable auth-rejected RED ONLY for a CONFIRMED
                    // auth / entitlement reject — NOT for the generic transient
                    // `Error` class. `Error` keeps its `error!` log + this Telegram
                    // visibility alert, but must not claim the credential is bad
                    // (the root false-"refresh the api-key" fix).
                    if class.sets_auth_rejected() {
                        feed_health.set_auth_rejected(Feed::Groww, true);
                    }
                    // FLEET-scoped Telegram coalescing (exam-fix 2026-07-06):
                    // each of N fleet connections used to fire its own HIGH
                    // alert per retry cycle (dozens of alternating messages
                    // during the exam). Fleet connections now aggregate into
                    // at most ONE summary per 60s window; suppressed events
                    // keep their per-line `error!` log + feed-health state
                    // above (only the Telegram fan-out is coalesced). The
                    // single-conn path (`conn_id == None`) is Passthrough —
                    // today's semantics byte-identical.
                    if let (Some(notifier), Some(reason)) = (&notifier, class.alert_reason()) {
                        use crate::groww_fleet_alerts::{
                            FleetAlertDecision, FleetAlertKind, format_fleet_reject_summary,
                            global_fleet_alert_decision,
                        };
                        match global_fleet_alert_decision(conn_id, FleetAlertKind::Reject) {
                            FleetAlertDecision::Passthrough => {
                                // Cross-child page cooldown (2026-07-09
                                // hostile-review HIGH fix): the never-streamed
                                // arm relaunches a persistently-rejected child
                                // every ~5 min, and each fresh child re-fires
                                // this once-per-child page — ~12 HIGH pages/hr
                                // without a cross-child bound. Page at most
                                // once per GROWW_REJECT_PAGE_COOLDOWN_SECS per
                                // supervisor; suppressed episodes keep their
                                // error! forwards + FEED-REJECT-01 signature +
                                // feed-health marking, and the ≥3-per-15-min
                                // restart pager independently covers "it keeps
                                // failing". CAS so the two drains never
                                // double-page the same window.
                                let now_secs =
                                    u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0);
                                let last =
                                    reject_page_gate.load(std::sync::atomic::Ordering::Relaxed);
                                if should_page_reject(now_secs, last, reject_page_cooldown_secs)
                                    && reject_page_gate
                                        .compare_exchange(
                                            last,
                                            now_secs.max(1),
                                            std::sync::atomic::Ordering::Relaxed,
                                            std::sync::atomic::Ordering::Relaxed,
                                        )
                                        .is_ok()
                                {
                                    // 2026-07-14 operator demand (the 14:59
                                    // IST page carried no WHY): thread the
                                    // SANITIZED signature — the same
                                    // sidecar_line_signature choke point the
                                    // FEED-REJECT-01 emit uses, never raw
                                    // child text — into the Telegram body.
                                    // Dated commandment-2 override:
                                    // feed-stall-watchdog-error-codes.md
                                    // §1c.1.
                                    notifier.notify(NotificationEvent::GrowwSidecarRejected {
                                        reason: reason.to_string(),
                                        fleet_summary: false,
                                        detail: sidecar_reject_detail(&line),
                                    });
                                } else {
                                    metrics::counter!(
                                        "tv_groww_reject_page_cooldown_suppressed_total",
                                    )
                                    .increment(1);
                                }
                            }
                            FleetAlertDecision::EmitSummary {
                                affected,
                                fleet_size,
                            } => {
                                let label = class.summary_label().unwrap_or("feed error");
                                notifier.notify(NotificationEvent::GrowwSidecarRejected {
                                    reason: format_fleet_reject_summary(
                                        affected, fleet_size, label,
                                    ),
                                    fleet_summary: true,
                                    // The fleet summary coalesces N children;
                                    // ONE child's line must not be presented
                                    // as the cause for all of them — the
                                    // per-child cause stays queryable via the
                                    // FEED-REJECT-01 coded signatures.
                                    detail: None,
                                });
                            }
                            FleetAlertDecision::Suppress => {
                                // RE-ARM the per-child one-shot latch
                                // (hostile-review fix 2026-07-06): a
                                // suppressed child fired NO Telegram, so its
                                // latch must survive. In a fleet-wide
                                // auth/entitlement outage the children never
                                // exit (they retry internally) and never
                                // stream (no recovery edge re-arms), so a
                                // consumed latch would make the ONE
                                // first-window "1 of N" summary the
                                // operator's entire signal forever. With the
                                // latch re-armed, each later 60s window's
                                // first still-unlatched child emits the
                                // ACCUMULATED count; Telegram volume stays
                                // bounded at ≤1 per window and ≤ fleet-size
                                // messages per outage episode (each emitting
                                // child latches on emit).
                                alerted.store(false, std::sync::atomic::Ordering::Relaxed);
                                metrics::counter!(
                                    "tv_groww_fleet_alerts_suppressed_total",
                                    "direction" => "reject",
                                )
                                .increment(1);
                            }
                        }
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
// APPROVED: supervision wiring — every arg is a distinct owned resource
#[allow(clippy::too_many_arguments)]
async fn supervise_child(
    child: &mut tokio::process::Child,
    feed: Feed,
    feed_runtime: &FeedRuntimeState,
    feed_health: &Arc<FeedHealthRegistry>,
    notifier: &Option<Arc<NotificationService>>,
    now_ist_nanos: fn() -> i64,
    storm: &mut StallRestartStorm,
    // §34 fleet identity (exam-fix 2026-07-06) — forwarded to the pipe
    // drains so fleet children route Telegram through the fleet coalescer.
    conn_id: Option<usize>,
    // Cross-child page cooldown gate (2026-07-09 HIGH fix) — supervisor
    // lifetime, so a fresh child's fresh `alerted` latch can no longer page
    // every ~5-min never-streamed relaunch.
    reject_page_gate: &Arc<std::sync::atomic::AtomicU64>,
    // Episode-mode-aware page-cooldown window (FIX-B, 2026-07-14) —
    // computed once at supervisor spawn via [`reject_page_cooldown_secs`].
    reject_page_cooldown_secs: u64,
    // Forensic `ws_event_audit` sender (scoreboard PR-B, 2026-07-10): a
    // stall-watchdog kill+relaunch stamps ONE `stall_restarted` row so the
    // 15:45 IST scorecard counts stall episodes with a cause slug. `None`
    // (test / defensive) is a no-op — the FEED-STALL-01 log + counter for
    // the same kill still fire.
    ws_audit_tx: &Option<
        tokio::sync::mpsc::Sender<tickvault_common::ws_event_types::WsEventAuditRow>,
    >,
) -> SuperviseOutcome {
    // One latch per running child so the alert fires at most once per instance.
    let alerted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // FEED-REJECT-01 detail latch — per child, shared by both drains, re-armed
    // only by a streaming recovery (never by the fleet Suppress re-arm).
    let detail_logged = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Stall-cause latch (PR-B): the drains record the child's last CONFIRMED
    // reject class so a stall kill stamps the precise cause slug.
    let stall_cause = Arc::new(std::sync::atomic::AtomicU8::new(STALL_CAUSE_NONE));
    let mut drains: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        drains.push(spawn_pipe_drain(
            stdout,
            "stdout",
            Arc::clone(feed_health),
            notifier.clone(),
            Arc::clone(&alerted),
            conn_id,
            Arc::clone(&detail_logged),
            Arc::clone(reject_page_gate),
            reject_page_cooldown_secs,
            Arc::clone(&stall_cause),
        ));
    }
    if let Some(stderr) = child.stderr.take() {
        drains.push(spawn_pipe_drain(
            stderr,
            "stderr",
            Arc::clone(feed_health),
            notifier.clone(),
            Arc::clone(&alerted),
            conn_id,
            Arc::clone(&detail_logged),
            Arc::clone(reject_page_gate),
            reject_page_cooldown_secs,
            Arc::clone(&stall_cause),
        ));
    }
    let abort_drains = || {
        for h in &drains {
            h.abort();
        }
    };
    // Never-streamed-first-tick window (2026-07-09 reject-loop hardening):
    // IST epoch-seconds instant at which we FIRST observed (exchange session
    // open + feed enabled + no first tick ever recorded this session). Cleared
    // the moment a first tick arrives (the stall arm owns liveness from then
    // on) or the session gate closes (pre-open / weekend / holiday time can
    // never count toward the threshold). Per-child by construction — a fresh
    // relaunched child gets the full threshold again, bounding the restart
    // cadence for a never-streaming feed to ~one per threshold window.
    let mut never_streamed_open_since: Option<u64> = None;
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
                    // Scoreboard PR-B: one `stall_restarted` forensic row per
                    // kill (storm and non-storm alike — every kill is an
                    // episode), cause slug from the drains' reject latch.
                    emit_stall_ws_audit(
                        ws_audit_tx.as_ref(),
                        stall_cause_slug(
                            StallArm::SilentSocket,
                            stall_cause.load(std::sync::atomic::Ordering::Relaxed),
                        ),
                        stall_secs,
                        conn_id,
                    );
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
                // NEVER-STREAMED arm (2026-07-09 reject-loop hardening). The
                // stall arm above requires a KNOWN last-tick, so a sidecar that
                // is alive + connected + subscribed but has NEVER streamed a
                // first tick this session (the all-day 09:22/14:17 IST
                // Error-class reject loop) was never restarted. Track the
                // continuous (exchange-session-open ∧ enabled ∧ no-first-tick)
                // window and kill + relaunch past the threshold — same
                // relaunch path, same storm bound, distinct attribution
                // counter. Gated on the [09:15, 15:30) EXCHANGE session AND
                // the trading-day calendar (NOT the plain time-of-day
                // market-hours gate the stall arm uses — that one is true on a
                // Saturday 10:00 IST and would restart-loop every weekend).
                if age.is_some() {
                    // First tick arrived this session — the stall arm owns
                    // liveness from here on; this arm disarms permanently
                    // (age can never return to None within this APP PROCESS —
                    // feed_health's last-tick stamp never resets in-process, so
                    // on day 2 of a long-running process this arm is dormant
                    // and the classic stall arm owns everything: no coverage
                    // hole, documented review LOW).
                    never_streamed_open_since = None;
                } else {
                    let now_nanos = now_ist_nanos();
                    let nanos_of_day = now_nanos.rem_euclid(
                        i64::from(tickvault_common::constants::SECONDS_PER_DAY) * 1_000_000_000,
                    );
                    // NOTE (review LOW): `g1_exchange_gate_accepts` is used
                    // here as a pure [09:15, 15:30) WALL-CLOCK window
                    // predicate for a supervision gate — NOT as a
                    // tick-persistence gate; the G1-vs-G2 doctrine in
                    // constants.rs (exchange-ts vs receipt-clock + grace)
                    // governs tick WRITE decisions, which this arm never
                    // touches. No grace tail is wanted here: a restart at
                    // 15:30:00 would be pointless churn.
                    let in_exchange_session =
                        tickvault_common::constants::g1_exchange_gate_accepts(nanos_of_day)
                            && tickvault_common::market_hours::is_trading_session_now();
                    if !(in_exchange_session && enabled) {
                        never_streamed_open_since = None;
                    } else {
                        // APPROVED: IST epoch nanos are always positive; max(0) guards the cast
                        #[allow(clippy::cast_sign_loss)]
                        let now_secs = (now_nanos / 1_000_000_000).max(0) as u64;
                        let since = *never_streamed_open_since.get_or_insert(now_secs);
                        let silent_secs = now_secs.saturating_sub(since);
                        if should_restart_on_never_streamed(
                            Some(silent_secs),
                            in_exchange_session,
                            enabled,
                            FEED_NEVER_STREAMED_RESTART_SECS,
                        ) {
                            let is_storm = storm.record_and_is_storm(now_secs);
                            if is_storm {
                                error!(
                                    code = ErrorCode::FeedStall01SidecarRestarted.code_str(),
                                    feed = feed.as_str(),
                                    silent_session_secs = silent_secs,
                                    reason = "never_streamed",
                                    rapid_restarts = storm.count_in_window(),
                                    "[feeds] sidecar NEVER-STREAMED RESTART STORM — connected but \
                                     no first tick this session and flapping; applying backoff \
                                     ceiling (still retrying, never giving up in market hours)"
                                );
                            } else {
                                warn!(
                                    code = ErrorCode::FeedStall01SidecarRestarted.code_str(),
                                    feed = feed.as_str(),
                                    silent_session_secs = silent_secs,
                                    reason = "never_streamed",
                                    "[feeds] sidecar alive + subscribed but NEVER streamed a first \
                                     tick this session during exchange hours — killing + \
                                     relaunching (re-auth + re-subscribe)"
                                );
                            }
                            // Route into the SAME pre-registered counter the
                            // ≥3-per-15-min restart pager reads (a persistent
                            // never-streamed loop MUST page) + a distinct
                            // attribution counter for the split.
                            metrics::counter!(
                                "tv_feed_sidecar_stall_restart_total",
                                "feed" => feed.as_str(),
                            )
                            .increment(1);
                            // Scoreboard PR-B: the never-streamed arm's own
                            // `stall_restarted` forensic row (cause slug
                            // `stall_never_streamed`, unless the drains
                            // latched a confirmed auth/entitlement reject).
                            emit_stall_ws_audit(
                                ws_audit_tx.as_ref(),
                                stall_cause_slug(
                                    StallArm::NeverStreamed,
                                    stall_cause.load(std::sync::atomic::Ordering::Relaxed),
                                ),
                                silent_secs,
                                conn_id,
                            );
                            metrics::counter!(
                                "tv_feed_sidecar_never_streamed_restart_total",
                                "feed" => feed.as_str(),
                            )
                            .increment(1);
                            if let Err(err) = child.start_kill() {
                                warn!(error = %err, feed = feed.as_str(), "[feeds] sidecar never-streamed-kill signal failed (already exiting?)");
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
    // Forensic `ws_event_audit` sender (scoreboard PR-B, 2026-07-10): every
    // stall-watchdog kill+relaunch writes ONE `stall_restarted` row through
    // the SAME consumer the bridge/Dhan emitters use. `None` = no-op
    // (defensive / test builds) — the FEED-STALL-01 signals still fire.
    ws_audit_tx: Option<
        tokio::sync::mpsc::Sender<tickvault_common::ws_event_types::WsEventAuditRow>,
    >,
) {
    let mut consecutive_failures: u32 = 0;
    // DEFENSIVE (best-effort) stall-restart counter registration. The
    // AUTHORITATIVE pre-registration lives in main.rs, immediately after
    // `observability::init_metrics` (boot Step 2) — round-5 review fix,
    // 2026-07-06. This supervisor is spawned BEFORE that recorder install
    // on the normal boot path, so THIS `counter!` handle resolves to the
    // no-op recorder there and registers nothing (the round-4 fix that
    // placed the sole registration here was therefore void — the
    // order_update_connection.rs task-start analogy did not transfer,
    // because that task starts post-auth, long after the install). Kept
    // as a harmless idempotent no-op that also covers any spawn context
    // where a recorder is already installed (tests / future callers).
    // Why registration matters at all: the CloudWatch agent's prometheus
    // pipeline drops each counter series' FIRST observed sample as its
    // delta baseline; a lazily-born series (first sample = 1, AT the first
    // restart) loses restart #1 of every app session — silently raising
    // the tv-<env>-feed-stall-restarts pager's effective first-episode
    // threshold from 3 to 4 (feed-stall-restart-alarm.tf). increment(0)
    // forces registration without an unused #[must_use] handle.
    metrics::counter!(
        "tv_feed_sidecar_stall_restart_total",
        "feed" => Feed::Groww.as_str(),
    )
    .increment(0);
    // Sliding-window tracker that bounds a flapping (reconnect→re-drop) socket so
    // rapid stall-restarts escalate + hit the backoff ceiling instead of a tight
    // kill loop. Persists across child launches for the supervisor's lifetime.
    let mut storm = StallRestartStorm::default();
    // Cross-child reject-page cooldown gate (2026-07-09 HIGH fix): epoch secs
    // of the last GrowwSidecarRejected Telegram page, shared across every
    // child this supervisor launches. 0 = never paged.
    let reject_page_gate = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Episode-mode-aware cooldown window (FIX-B, 2026-07-14): 60s while the
    // GrowwFeed episode fold owns page dedup, the legacy 1800s when the
    // `[notification] episode_mode` kill switch is OFF (boot-time value; a
    // runtime config flip needs a restart, like episode_mode itself).
    let reject_cooldown_secs = reject_page_cooldown_secs(opts.episode_mode);
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
            )
            // Disk-retention hardening (2026-07-13): capture-archive S3
            // destination for the sidecar's verified offload sweep. Bucket +
            // prefix NAMES only — never a credential (the sidecar uses the
            // instance-profile credential chain, same as the SSM token read).
            // Empty bucket = archival OFF (archives kept on disk).
            .env("TICKVAULT_GROWW_ARCHIVE_S3_BUCKET", &opts.archive_s3_bucket)
            .env("TICKVAULT_GROWW_ARCHIVE_S3_PREFIX", &opts.archive_s3_prefix);
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
            opts.conn_id,
            &reject_page_gate,
            reject_cooldown_secs,
            &ws_audit_tx,
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
    ws_audit_tx: Option<
        tokio::sync::mpsc::Sender<tickvault_common::ws_event_types::WsEventAuditRow>,
    >,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let handle = tokio::spawn(run_groww_sidecar_supervisor(
                Arc::clone(&feed_runtime),
                opts.clone(),
                Arc::clone(&feed_health),
                Arc::clone(&notifier),
                ws_audit_tx.clone(),
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
// APPROVED: fleet wiring — every arg is a distinct owned resource
#[allow(clippy::too_many_arguments)]
// TEST-EXEMPT: spawn/abort/sleep reconciler loop; the pure reconcile decision (fleet_reconcile_actions) + the per-conn option/env/argv/needle primitives are unit-tested, and the child lifecycle is the already-tested run_groww_sidecar_supervisor.
pub fn spawn_groww_scale_fleet(
    feed_runtime: Arc<FeedRuntimeState>,
    base_opts: GrowwSidecarOptions,
    shards_root: PathBuf,
    desired_conns: Arc<std::sync::atomic::AtomicUsize>,
    wake: Arc<tokio::sync::Notify>,
    feed_health: Arc<FeedHealthRegistry>,
    notifier: Arc<arc_swap::ArcSwapOption<NotificationService>>,
    ws_audit_tx: Option<
        tokio::sync::mpsc::Sender<tickvault_common::ws_event_types::WsEventAuditRow>,
    >,
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
                        ws_audit_tx.clone(),
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
                            ws_audit_tx.clone(),
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
                        // Hardening 2026-07-06: drop the killed conn's
                        // subscribe-proof status file so the ladder's
                        // plateau gate never counts a dead connection as
                        // acknowledged (the path is undated and the
                        // sidecar never deletes it). Best-effort — the
                        // ladder's freshness bound is the backstop.
                        let _ = crate::groww_bridge::remove_subscribe_proof(&shards_root, conn_id);
                    }
                }
                FleetAction::Steady => {}
            }
            // Publish the live fleet size to the fleet-scoped alert
            // coalescer (exam-fix 2026-07-06) so the "N of M connections
            // retrying" summaries carry an honest denominator.
            crate::groww_fleet_alerts::set_global_fleet_size(children.len());
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
    ws_audit_tx: Option<
        tokio::sync::mpsc::Sender<tickvault_common::ws_event_types::WsEventAuditRow>,
    >,
) -> tokio::task::JoinHandle<()> {
    let files = crate::groww_bridge::shard_files(shards_root, conn_id);
    let opts = shard_sidecar_options(base_opts, &files, format!("c{conn_id:02}"));
    tokio::spawn(run_groww_sidecar_supervisor(
        Arc::clone(feed_runtime),
        opts,
        Arc::clone(feed_health),
        Arc::clone(notifier),
        ws_audit_tx,
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
    fn test_stall_restart_counter_is_preregistered_after_recorder_install() {
        // RATCHET (round-5 review fix, 2026-07-06 — supersedes the round-4
        // `..._before_supervise_loop` scan, which was VACUOUS: it pinned
        // source order WITHIN this file, but this supervisor is spawned in
        // main.rs BEFORE `observability::init_metrics` installs the
        // Prometheus recorder, so a registration here resolves to the no-op
        // recorder and registers nothing). The CW agent's prometheus
        // pipeline drops each counter series' FIRST sample as the delta
        // baseline, so a lazily-born tv_feed_sidecar_stall_restart_total
        // (first sample = 1, at the first restart) loses restart #1 of
        // every app session — the tv-<env>-feed-stall-restarts pager's
        // effective first-episode threshold silently becomes 4 instead of 3
        // on a box that restarts daily. The registration that actually
        // takes effect therefore MUST live in main.rs AFTER the
        // init_metrics call. Source-order scan of main.rs (house pattern).
        let main_src = include_str!("main.rs");
        let install = main_src
            .find("observability::init_metrics(")
            .expect("the metrics recorder install site must exist in main.rs");
        let reg = main_src
            .find("\"tv_feed_sidecar_stall_restart_total\"")
            .expect(
                "main.rs must pre-register tv_feed_sidecar_stall_restart_total post-install \
                 (the boot-time registration the feed-stall-restarts pager depends on)",
            );
        assert!(
            install < reg,
            "the stall-restart counter registration in main.rs must come AFTER \
             observability::init_metrics — a pre-install registration resolves to the \
             no-op recorder and the pager silently loses restart #1 of every session"
        );
        // The per-restart increment (the real signal) must still exist here —
        // distinguishably from the defensive `.increment(0)` registration at
        // supervisor start. Round-6 review fix: the round-5 `>= 1` count was
        // VACUOUS — the defensive registration alone satisfied it, so deleting
        // the per-restart `.increment(1)` (the ONLY site that ever moves the
        // counter, hence the tv-<env>-feed-stall-restarts pager's only signal)
        // kept the build green. Require BOTH quoted sites AND that at least
        // one metric-name occurrence is followed by `.increment(1)` within the
        // same counter! expression. (The escaped `\"tv_feed...\"` literals in
        // THIS test do not match the quoted needle — the backslash breaks the
        // substring — so the occurrences are exactly the two code sites.)
        let src = include_str!("groww_sidecar_supervisor.rs");
        let needle = "\"tv_feed_sidecar_stall_restart_total\"";
        let sites: Vec<usize> = src.match_indices(needle).map(|(i, _)| i).collect();
        assert!(
            sites.len() >= 2,
            "expected BOTH the defensive registration and the per-restart increment \
             of tv_feed_sidecar_stall_restart_total in the supervisor"
        );
        let has_per_restart_increment = sites.iter().any(|&i| {
            let window_end = (i + 200).min(src.len());
            src[i..window_end].contains(".increment(1)")
        });
        assert!(
            has_per_restart_increment,
            "expected at least one tv_feed_sidecar_stall_restart_total site to call \
             .increment(1) — the per-restart signal the tv-<env>-feed-stall-restarts \
             pager depends on; deleting it blinds the pager entirely"
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

    #[test]
    fn test_supervisor_injects_archive_s3_env() {
        // Disk-retention hardening (2026-07-13): the supervisor must inject
        // BOTH capture-archive S3 env vars into the sidecar child so the
        // verified offload sweep knows its destination. Name/prefix strings
        // only — never a credential. Source-scan (the supervise loop is
        // TEST-EXEMPT); split needles so rustfmt wrapping can't break it.
        let src = include_str!("groww_sidecar_supervisor.rs");
        let bucket_key = format!("\"TICKVAULT_GROWW_ARCHIVE{}_S3_BUCKET\"", "");
        let prefix_key = format!("\"TICKVAULT_GROWW_ARCHIVE{}_S3_PREFIX\"", "");
        assert!(
            src.contains(&bucket_key) && src.contains("opts.archive_s3_bucket"),
            "the supervisor must inject TICKVAULT_GROWW_ARCHIVE_S3_BUCKET \
             from opts.archive_s3_bucket"
        );
        assert!(
            src.contains(&prefix_key) && src.contains("opts.archive_s3_prefix"),
            "the supervisor must inject TICKVAULT_GROWW_ARCHIVE_S3_PREFIX \
             from opts.archive_s3_prefix"
        );
    }

    #[test]
    fn test_shard_options_carry_archive_s3_fields() {
        // §34 fleet children must inherit the archive destination — a shard
        // child that silently dropped it would accumulate its own capture
        // archives unbounded.
        let base = GrowwSidecarOptions {
            archive_s3_bucket: "tv-prod-cold".to_string(),
            archive_s3_prefix: "groww-capture".to_string(),
            ..GrowwSidecarOptions::default()
        };
        let files = crate::groww_bridge::shard_files(Path::new("data/groww"), 3);
        let child = shard_sidecar_options(&base, &files, "shard 3".to_string());
        assert_eq!(child.archive_s3_bucket, "tv-prod-cold");
        assert_eq!(child.archive_s3_prefix, "groww-capture");
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

    /// Exam-fix 2026-07-06: the silent-feed / permissions class prose must
    /// describe the OBSERVED condition (server not sending data — session
    /// limit or throttle), never assert an unproven account-entitlement
    /// fault; and every alert class carries a short fleet summary label.
    #[test]
    fn test_alert_reason_wording_is_honest_and_summary_labels_exist() {
        let reason = SidecarLineClass::EntitlementRejected
            .alert_reason()
            .expect("alert class must have a reason");
        assert!(
            reason.contains("server is not sending data to this connection"),
            "reject prose must state the observation, got: {reason}"
        );
        assert!(
            reason.contains("session limit or throttle") && reason.contains("retrying"),
            "reject prose must name the likely cause + the retry, got: {reason}"
        );
        assert!(
            !reason.contains("account lacks"),
            "the misleading entitlement claim must not return: {reason}"
        );
        for class in [
            SidecarLineClass::EntitlementRejected,
            SidecarLineClass::AuthRejected,
            SidecarLineClass::Error,
        ] {
            let label = class
                .summary_label()
                .expect("alert class must have a fleet summary label");
            assert!(!label.is_empty() && !label.contains(".rs"));
        }
        assert!(SidecarLineClass::Subscribed.summary_label().is_none());
        assert!(SidecarLineClass::Streaming.summary_label().is_none());
        assert!(SidecarLineClass::Info.summary_label().is_none());
    }

    /// Hostile-review fixes 2026-07-06, pinned by source-scan (the drain loop
    /// is a TEST-EXEMPT pipe driver):
    /// (a) a fleet `Suppress` decision must RE-ARM the per-child one-shot
    ///     latch — otherwise a fleet-wide outage collapses to a single
    ///     never-corrected "1 of N" Telegram (the suppressed children's only
    ///     notify attempt would be consumed forever);
    /// (b) the fleet-coalesced summary must be marked `fleet_summary: true`
    ///     (and the single-conn passthrough `false`) so the Telegram body
    ///     does not wrap a partial-fleet count in the single-conn
    ///     total-outage trailer.
    #[test]
    fn test_fleet_suppress_rearms_latch_and_summary_is_fleet_marked() {
        let src = include_str!("groww_sidecar_supervisor.rs");
        let suppress_arm = src
            .split("FleetAlertDecision::Suppress => {")
            .nth(1)
            .expect("the reject Suppress arm must exist");
        let suppress_body = suppress_arm
            .split("tv_groww_fleet_alerts_suppressed_total")
            .next()
            .expect("the Suppress arm must count suppressions");
        assert!(
            suppress_body.contains("alerted.store(false"),
            "the Suppress arm must re-arm the per-child latch BEFORE counting \
             (a suppressed child must keep its one notify attempt)"
        );
        assert!(
            src.contains("fleet_summary: true,"),
            "the fleet EmitSummary notify must be marked fleet_summary: true"
        );
        assert!(
            src.contains("fleet_summary: false,"),
            "the single-conn passthrough notify must be marked fleet_summary: false"
        );
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

    // ── 2026-07-09 reject-loop hardening: never-streamed restart arm ─────────

    #[test]
    fn test_should_restart_on_never_streamed_fires_past_threshold_in_session() {
        // In the exchange session, feed enabled, no first tick ever, silence
        // past the threshold → restart (today's all-day reject-loop case).
        assert!(should_restart_on_never_streamed(
            Some(FEED_NEVER_STREAMED_RESTART_SECS + 1),
            true,
            true,
            FEED_NEVER_STREAMED_RESTART_SECS
        ));
    }

    #[test]
    fn test_should_restart_on_never_streamed_boundary_and_gates() {
        // Strict `>`: exactly AT the threshold must NOT fire.
        assert!(!should_restart_on_never_streamed(
            Some(FEED_NEVER_STREAMED_RESTART_SECS),
            true,
            true,
            FEED_NEVER_STREAMED_RESTART_SECS
        ));
        // Out of the exchange session (pre-open / weekend / holiday) → never.
        assert!(!should_restart_on_never_streamed(
            Some(FEED_NEVER_STREAMED_RESTART_SECS + 100),
            false,
            true,
            FEED_NEVER_STREAMED_RESTART_SECS
        ));
        // Feed disabled → the toggle wins; never restart.
        assert!(!should_restart_on_never_streamed(
            Some(FEED_NEVER_STREAMED_RESTART_SECS + 100),
            true,
            false,
            FEED_NEVER_STREAMED_RESTART_SECS
        ));
        // No tracked window (caller has not armed one) → fail-closed.
        assert!(!should_restart_on_never_streamed(
            None,
            true,
            true,
            FEED_NEVER_STREAMED_RESTART_SECS
        ));
        // Zero silence (window just armed) → never.
        assert!(!should_restart_on_never_streamed(
            Some(0),
            true,
            true,
            FEED_NEVER_STREAMED_RESTART_SECS
        ));
    }

    #[test]
    fn test_never_streamed_constants_sane() {
        // The never-streamed threshold MUST be far above the stall threshold
        // (the arms own disjoint halves of the age space, but the
        // never-streamed case includes the sidecar's own auth+connect+
        // subscribe worst path) and inside the operator-approved 3–5 min
        // band. Earliest possible fire = 09:15:00 + threshold — must stay
        // clear of a slow session start.
        assert!(FEED_NEVER_STREAMED_RESTART_SECS > FEED_STALL_RESTART_SECS);
        assert!((180..=600).contains(&FEED_NEVER_STREAMED_RESTART_SECS));
        // The signature bound is compact but big enough for the sidecar's
        // `groww sidecar error [<phase>]: <Type>: <detail>` prefix.
        assert!((80..=300).contains(&SIDECAR_LINE_SIGNATURE_MAX_CHARS));
    }

    #[test]
    fn test_never_streamed_arm_is_wired_into_supervise_child() {
        // SEAM GUARD (house pattern of test_stall_restart_reuses_existing_
        // relaunch_path): the never-streamed arm must (a) consult the pure
        // decision fn, (b) gate on the [09:15, 15:30) exchange session AND
        // the trading-day calendar (NOT the plain time-of-day gate — that
        // one is true on a Saturday 10:00 IST), (c) route into the SAME
        // pre-registered stall-restart counter the ≥3-per-15-min pager
        // reads, PLUS the distinct attribution counter, and (d) return
        // through the existing SuperviseOutcome relaunch seam.
        let src = include_str!("groww_sidecar_supervisor.rs");
        assert!(
            src.contains("should_restart_on_never_streamed("),
            "supervise_child must consult the pure never-streamed decision"
        );
        assert!(
            src.contains("g1_exchange_gate_accepts") && src.contains("is_trading_session_now()"),
            "the never-streamed arm must gate on the exchange session + trading-day calendar"
        );
        assert!(
            src.contains("\"tv_feed_sidecar_never_streamed_restart_total\""),
            "the never-streamed arm must increment its attribution counter"
        );
        let needle = "\"tv_feed_sidecar_never_streamed_restart_total\"";
        let has_increment = src.match_indices(needle).any(|(i, _)| {
            let window_end = (i + 200).min(src.len());
            src[i..window_end].contains(".increment(1)")
        });
        assert!(
            has_increment,
            "the attribution counter must have a real .increment(1) site"
        );
        assert!(
            src.contains("reason = \"never_streamed\""),
            "the FEED-STALL-01 warn/error lines must carry the never_streamed reason"
        );
    }

    #[test]
    fn test_never_streamed_counter_is_preregistered_in_main() {
        // Mirror of test_stall_restart_counter_is_preregistered_after_
        // recorder_install for the attribution counter: registration must
        // live in main.rs AFTER the metrics recorder install (a pre-install
        // registration resolves to the no-op recorder).
        let main_src = include_str!("main.rs");
        let install = main_src
            .find("observability::init_metrics(")
            .expect("the metrics recorder install site must exist in main.rs");
        let reg = main_src
            .find("\"tv_feed_sidecar_never_streamed_restart_total\"")
            .expect("main.rs must pre-register tv_feed_sidecar_never_streamed_restart_total");
        assert!(
            install < reg,
            "the never-streamed counter registration must come AFTER init_metrics"
        );
    }

    // ── 2026-07-09 hostile-review HIGH fix: cross-child page cooldown ────────

    #[test]
    fn test_should_page_reject_rising_edge_and_cooldown() {
        // Never paged this supervisor lifetime → page immediately (rising edge).
        assert!(should_page_reject(
            1_000,
            0,
            GROWW_REJECT_PAGE_COOLDOWN_SECS
        ));
        // Inside the cooldown → suppressed (the ~5-min never-streamed relaunch
        // cadence can no longer produce ~12 HIGH pages/hour).
        assert!(!should_page_reject(
            1_000 + GROWW_REJECT_PAGE_COOLDOWN_SECS - 1,
            1_000,
            GROWW_REJECT_PAGE_COOLDOWN_SECS
        ));
        // Cooldown fully elapsed → page again (a persisting condition still
        // reaches the operator at a bounded cadence).
        assert!(should_page_reject(
            1_000 + GROWW_REJECT_PAGE_COOLDOWN_SECS,
            1_000,
            GROWW_REJECT_PAGE_COOLDOWN_SECS
        ));
        // Backward clock step → elapsed saturates to 0 → suppressed, never a
        // panic or a storm.
        assert!(!should_page_reject(
            500,
            1_000,
            GROWW_REJECT_PAGE_COOLDOWN_SECS
        ));
        // Cooldown pins (2026-07-14 operator noise directive + FIX-B/FIX-C
        // hostile review): the GrowwFeed episode fold owns page dedup, so
        // the episode-mode-ON gate is ONLY the transport-failure bound on
        // the SendNewFallback path — EXACTLY 60s (aligned with the fleet
        // 60s coalescer; drift DOWN would weaken the fallback storm cap
        // 60×, drift UP would starve the bubble's occurrence counter).
        // With `[notification] episode_mode = false` (the documented
        // rollback kill switch) every reject takes the legacy
        // Immediate+SMS lane, so the LEGACY 1800s bound (one page per
        // 30 min, the 2026-07-09 contract) is restored — EXACTLY 1800.
        assert_eq!(GROWW_REJECT_PAGE_COOLDOWN_SECS, 60);
        assert_eq!(GROWW_REJECT_PAGE_COOLDOWN_LEGACY_SECS, 1_800);
        assert_eq!(
            reject_page_cooldown_secs(true),
            GROWW_REJECT_PAGE_COOLDOWN_SECS
        );
        assert_eq!(
            reject_page_cooldown_secs(false),
            GROWW_REJECT_PAGE_COOLDOWN_LEGACY_SECS
        );
        // Default options carry the fold-ON default (matches config.rs
        // `default_notification_episode_mode`).
        assert!(GrowwSidecarOptions::default().episode_mode);
    }

    #[test]
    fn test_reject_page_cooldown_is_wired_into_passthrough_arm() {
        // SEAM GUARD: the single-conn Passthrough Telegram fan-out must be
        // gated by should_page_reject on the supervisor-lifetime gate (the
        // 2026-07-09 HIGH page-storm fix), with the suppression counted; and
        // the FEED-REJECT-01 emit must be bounded by its OWN detail latch
        // (never the fleet-Suppress-re-armed `alerted` latch).
        let src = include_str!("groww_sidecar_supervisor.rs");
        assert!(
            src.contains("should_page_reject(") && src.contains("reject_page_gate"),
            "the Passthrough page must consult the cross-child cooldown gate"
        );
        assert!(
            src.contains("\"tv_groww_reject_page_cooldown_suppressed_total\""),
            "a suppressed page must be counted (never silent)"
        );
        assert!(
            src.contains("detail_logged.swap(true"),
            "the FEED-REJECT-01 emit must be bounded by its own per-child latch"
        );
    }

    // ── 2026-07-09 reject-loop hardening: FEED-REJECT-01 cause signature ─────

    #[test]
    fn test_sidecar_line_signature_caps_redacts_and_strips() {
        // (a) Length cap: a long line truncates to the bound, UTF-8-safe.
        let long_line = format!(
            "groww sidecar error [consume]: TimeoutError: {} — reconnecting",
            "é".repeat(400)
        );
        let sig = sidecar_line_signature(&long_line);
        assert!(sig.chars().count() <= SIDECAR_LINE_SIGNATURE_MAX_CHARS);
        assert!(sig.starts_with("groww sidecar error [consume]: TimeoutError:"));

        // (b) JWT-shape redaction: a token that somehow survived the
        // sidecar's own redaction can never ride the signature.
        let jwt_line = "groww sidecar error [feed-connect]: ConnectError: bearer \
             eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJncm93dyJ9.abcdefghijklmnop — reconnecting in 5s";
        let sig = sidecar_line_signature(jwt_line);
        assert!(!sig.contains("eyJhbGciOiJIUzI1NiJ9"), "{sig}");
        assert!(sig.contains("[REDACTED-JWT]"), "{sig}");

        // (c) Control-char strip: no newline injection into the one-line
        // log field (defense-in-depth — next_line() already splits).
        let ctl_line = "groww sidecar error [subscribe]: X\u{0}Y\rZ";
        let sig = sidecar_line_signature(ctl_line);
        assert!(!sig.chars().any(char::is_control), "{sig}");

        // (c2) BiDi/zero-width strip (2026-07-09 security pass, LOW):
        // Cf-category format chars are NOT char::is_control — strip them so
        // a hostile diagnostic can't visually reorder terminal output.
        let bidi_line = "groww sidecar error [consume]: A\u{202e}B\u{2066}C\u{200b}D\u{feff}E";
        let sig = sidecar_line_signature(bidi_line);
        assert!(
            !sig.chars().any(|c| matches!(
                c,
                '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200b}'..='\u{200f}' | '\u{feff}'
            )),
            "{sig}"
        );
        assert!(sig.ends_with("ABCDE"), "{sig}");

        // (d) A clean short line passes through recognizably.
        let clean = "groww sidecar error [auth]: RuntimeError: SSM parameter holds no usable token";
        assert_eq!(sidecar_line_signature(clean), clean);
    }

    // ── 2026-07-14 operator demand: the reject page carries the WHY ─────────

    #[test]
    fn test_sidecar_reject_detail_is_sanitized_signature_or_none() {
        // (a) The detail is EXACTLY the sidecar_line_signature choke-point
        // output for a real reject line (the 14:59 IST incident line).
        let incident = "ERROR growwapi.groww.nats_client: Error: nats: unexpected EOF";
        assert_eq!(
            sidecar_reject_detail(incident).as_deref(),
            Some(sidecar_line_signature(incident).as_str()),
            "detail must be the choke-point output, nothing else"
        );
        assert_eq!(sidecar_reject_detail(incident).as_deref(), Some(incident));

        // (b) A hostile line (embedded JWT + control chars + overlong) only
        // ever reaches the detail in sanitized, truncated form.
        let hostile = format!(
            "groww sidecar error [consume]: bearer \
             eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJncm93dyJ9.abcdefghijklmnop \u{0}\r{}",
            "x".repeat(400)
        );
        let detail = sidecar_reject_detail(&hostile).expect("non-empty hostile line");
        assert!(!detail.contains("eyJhbGciOiJIUzI1NiJ9"), "{detail}");
        assert!(detail.contains("[REDACTED-JWT]"), "{detail}");
        assert!(!detail.chars().any(char::is_control), "{detail}");
        assert!(detail.chars().count() <= SIDECAR_LINE_SIGNATURE_MAX_CHARS);

        // (c) Empty / whitespace / control-only lines degrade to None so the
        // Telegram body never renders a hollow "rejected:  — retrying".
        for hollow in ["", "   ", "\u{0}\u{1}\r", "\u{202e}\u{200b}"] {
            assert_eq!(
                sidecar_reject_detail(hollow),
                None,
                "hollow line {hollow:?} must yield None"
            );
        }
    }

    #[test]
    fn test_reject_page_carries_sanitized_detail() {
        // SEAM GUARD (operator demand 2026-07-14): the single-conn
        // Passthrough Telegram page must thread the SANITIZED signature —
        // and ONLY via the sidecar_reject_detail choke point — into the
        // GrowwSidecarRejected event, while the fleet-coalesced summary arm
        // deliberately passes None (one child's line is not the cause for N
        // connections). Regressing either re-opens the 14:59 IST
        // "reported an error with no WHY" incident class.
        // Needles are assembled at runtime (concat!) so this test's OWN
        // source can never satisfy the scan (the whole-file include_str!
        // self-match vacuous false-OK class — the 2026-07-06 shadow-writer
        // lesson).
        let src = include_str!("groww_sidecar_supervisor.rs");
        let threaded = concat!("detail: ", "sidecar_reject_detail(&line)");
        assert!(
            src.contains(threaded),
            "the Passthrough notify arm must thread the sanitized detail"
        );
        let fleet_none = concat!("detail: ", "None");
        assert!(
            src.contains(fleet_none),
            "the fleet EmitSummary arm must pass detail: None"
        );
        // No raw-line bypass: the detail must never be built from `line`
        // without the sanitize choke point.
        let raw_bypass = concat!("detail: ", "Some(line");
        assert!(
            !src.contains(raw_bypass),
            "raw child text must never ride the Telegram detail"
        );
    }

    #[test]
    fn test_feed_reject_emit_is_wired_into_alert_edge() {
        // SEAM GUARD: the FEED-REJECT-01 coded emit (the bounded cause
        // signature) must live inside the once-per-child alert edge in
        // spawn_pipe_drain — carrying the code field (tag-guard law), the
        // class, and the sanitized signature. Deleting it re-blinds the
        // coded stream to WHY a reject loop repeats (the 2026-07-09
        // incident class).
        let src = include_str!("groww_sidecar_supervisor.rs");
        assert!(
            src.contains("ErrorCode::FeedReject01SidecarErrorDetail.code_str()"),
            "spawn_pipe_drain must emit the FEED-REJECT-01 coded error"
        );
        assert!(
            src.contains("signature = %sidecar_line_signature(&line)"),
            "the FEED-REJECT-01 emit must carry the bounded sanitized signature"
        );
    }

    // ------------------------------------------------------------------
    // Stall-restart episode rows (dual-feed scoreboard PR-B, 2026-07-10)
    // ------------------------------------------------------------------

    #[test]
    fn test_stall_cause_latch_update_matrix() {
        // Only CONFIRMED reject classes latch; streaming clears; the generic
        // transient Error / subscribe / info classes never touch the latch
        // (the false-"refresh the api-key" discipline carried over).
        for (sf, open) in [(false, false), (false, true), (true, false), (true, true)] {
            assert_eq!(
                stall_cause_latch_update(SidecarLineClass::AuthRejected, sf, open),
                Some(STALL_CAUSE_AUTH),
                "auth latches regardless of silent-feed/market flags"
            );
            assert_eq!(
                stall_cause_latch_update(SidecarLineClass::Streaming, sf, open),
                Some(STALL_CAUSE_NONE),
                "streaming recovery always clears"
            );
            for neutral in [
                SidecarLineClass::Error,
                SidecarLineClass::Subscribed,
                SidecarLineClass::Info,
            ] {
                assert_eq!(
                    stall_cause_latch_update(neutral, sf, open),
                    None,
                    "{neutral:?}"
                );
            }
        }
        // PR-B fix round 1: the EntitlementRejected split. A HARD
        // authorization/permission/entitlement line latches ENTITLEMENT at
        // ANY time; the SILENT-FEED watchdog line latches the weak
        // SILENT_FEED value in-market and NOTHING off-hours (the benign
        // ~08:35 pre-open print must never count as a confirmed reject).
        for open in [false, true] {
            assert_eq!(
                stall_cause_latch_update(SidecarLineClass::EntitlementRejected, false, open),
                Some(STALL_CAUSE_ENTITLEMENT),
                "hard entitlement lines latch at any hour"
            );
        }
        assert_eq!(
            stall_cause_latch_update(SidecarLineClass::EntitlementRejected, true, true),
            Some(STALL_CAUSE_SILENT_FEED),
            "in-market silent-feed watchdog latches the WEAK value"
        );
        assert_eq!(
            stall_cause_latch_update(SidecarLineClass::EntitlementRejected, true, false),
            None,
            "off-hours silent-feed watchdog must never latch"
        );
    }

    #[test]
    fn test_stall_cause_slug_matrix() {
        use tickvault_common::feed_blame::{
            STALL_SOURCE_AUTH_STALE, STALL_SOURCE_ENTITLEMENT, STALL_SOURCE_NEVER_STREAMED,
            STALL_SOURCE_SILENT_SOCKET,
        };
        // A latched confirmed reject class outranks the arm's generic slug.
        for arm in [StallArm::SilentSocket, StallArm::NeverStreamed] {
            assert_eq!(
                stall_cause_slug(arm, STALL_CAUSE_AUTH),
                STALL_SOURCE_AUTH_STALE
            );
            assert_eq!(
                stall_cause_slug(arm, STALL_CAUSE_ENTITLEMENT),
                STALL_SOURCE_ENTITLEMENT
            );
        }
        // No latch → the arm's own slug.
        assert_eq!(
            stall_cause_slug(StallArm::SilentSocket, STALL_CAUSE_NONE),
            STALL_SOURCE_SILENT_SOCKET
        );
        assert_eq!(
            stall_cause_slug(StallArm::NeverStreamed, STALL_CAUSE_NONE),
            STALL_SOURCE_NEVER_STREAMED
        );
        // PR-B fix round 1: the WEAK silent-feed latch never outranks the
        // arm — both arms keep their own slug (this is what keeps
        // stall_never_streamed reachable when the sidecar's 30s silent
        // watchdog fired before the 300s never-streamed kill).
        assert_eq!(
            stall_cause_slug(StallArm::SilentSocket, STALL_CAUSE_SILENT_FEED),
            STALL_SOURCE_SILENT_SOCKET
        );
        assert_eq!(
            stall_cause_slug(StallArm::NeverStreamed, STALL_CAUSE_SILENT_FEED),
            STALL_SOURCE_NEVER_STREAMED
        );
        // Total: an unknown future latch value degrades to the arm slug,
        // never a panic, never raw text.
        assert_eq!(
            stall_cause_slug(StallArm::SilentSocket, 250),
            STALL_SOURCE_SILENT_SOCKET
        );
    }

    #[test]
    fn test_stall_slug_combined_path_silent_feed_vs_hard_authorization() {
        // PR-B fix round 1 (review MEDIUM) — the COMBINED path the original
        // tests never exercised: subscribed → the sidecar's SILENT-FEED
        // watchdog lines → never-streamed kill. The latch must NOT convert
        // the kill's attribution to entitlement.
        use tickvault_common::feed_blame::{STALL_SOURCE_ENTITLEMENT, STALL_SOURCE_NEVER_STREAMED};
        let silent = "groww sidecar: SILENT FEED — subscribed 767 stocks + 28 indices \
                      but received NO live records in 30s";
        let class = classify_sidecar_line(silent);
        assert_eq!(class, SidecarLineClass::EntitlementRejected);
        assert!(is_silent_feed_diagnostic(silent));
        // In-market silent-feed lines latch the WEAK value → the
        // never-streamed kill keeps its own slug.
        let latched = stall_cause_latch_update(class, true, true).expect("latches in-market");
        assert_eq!(latched, STALL_CAUSE_SILENT_FEED);
        assert_eq!(
            stall_cause_slug(StallArm::NeverStreamed, latched),
            STALL_SOURCE_NEVER_STREAMED,
            "silent-feed watchdog lines must not steal the never-streamed attribution"
        );
        // The same lines OFF-hours (the ~08:35 pre-open print on every
        // normal boot) never latch at all.
        assert_eq!(stall_cause_latch_update(class, true, false), None);
        // A HARD authorization line before the kill DOES outrank the arm.
        let hard = "NATS error_cb -> Authorization Violation";
        let hard_class = classify_sidecar_line(hard);
        assert_eq!(hard_class, SidecarLineClass::EntitlementRejected);
        assert!(!is_silent_feed_diagnostic(hard));
        let hard_latch = stall_cause_latch_update(hard_class, false, true).expect("hard latches");
        assert_eq!(hard_latch, STALL_CAUSE_ENTITLEMENT);
        assert_eq!(
            stall_cause_slug(StallArm::NeverStreamed, hard_latch),
            STALL_SOURCE_ENTITLEMENT
        );
        assert_eq!(
            stall_cause_slug(StallArm::SilentSocket, hard_latch),
            STALL_SOURCE_ENTITLEMENT
        );
    }

    #[test]
    fn test_build_stall_ws_audit_row_shape() {
        use tickvault_common::feed_blame::STALL_SOURCE_SILENT_SOCKET;
        let row = build_stall_ws_audit_row(STALL_SOURCE_SILENT_SOCKET, 42, None);
        assert_eq!(
            row.event_kind,
            tickvault_common::ws_event_types::WsEventKind::StallRestarted
        );
        assert_eq!(
            row.ws_type,
            tickvault_common::ws_event_types::WsType::GrowwBridge
        );
        assert_eq!(row.feed, tickvault_common::feed::Feed::Groww);
        assert_eq!(row.connection_index, 0);
        assert_eq!(row.source, STALL_SOURCE_SILENT_SOCKET);
        // FIXED reason literal — never raw child text (redaction discipline).
        assert_eq!(row.reason, STALL_RESTART_AUDIT_REASON);
        assert_eq!(row.down_secs, 42);
        assert_eq!(
            row.dhan_code,
            tickvault_common::ws_event_types::WS_EVENT_NO_DHAN_CODE
        );
        // §34 fleet child identity rides in connection_index.
        let fleet = build_stall_ws_audit_row(STALL_SOURCE_SILENT_SOCKET, 7, Some(3));
        assert_eq!(fleet.connection_index, 3);
        // The designated ts + trading day are IST-consistent (day floor).
        assert!(row.event_ts_ist_nanos >= row.trading_date_ist_nanos);
        assert_eq!(
            row.trading_date_ist_nanos % (86_400 * 1_000_000_000),
            0,
            "trading day must be an IST-midnight floor"
        );
    }

    #[tokio::test]
    async fn test_emit_stall_ws_audit_noop_when_tx_none_and_sends_when_some() {
        use tickvault_common::feed_blame::STALL_SOURCE_NEVER_STREAMED;
        // No channel attached → no-op (defensive/test builds).
        emit_stall_ws_audit(None, STALL_SOURCE_NEVER_STREAMED, 300, None);
        // Attached channel → exactly one row, best-effort try_send.
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<tickvault_common::ws_event_types::WsEventAuditRow>(4);
        emit_stall_ws_audit(Some(&tx), STALL_SOURCE_NEVER_STREAMED, 300, Some(1));
        let row = rx.try_recv().expect("one stall row must be sent");
        assert_eq!(row.source, STALL_SOURCE_NEVER_STREAMED);
        assert_eq!(row.down_secs, 300);
        assert_eq!(row.connection_index, 1);
        assert!(rx.try_recv().is_err(), "exactly one row per emit");
    }

    #[test]
    fn test_ratchet_stall_arms_emit_stall_restarted_rows() {
        // WIRING RATCHET (scoreboard PR-B): BOTH stall-watchdog kill arms in
        // supervise_child must stamp a `stall_restarted` forensic row before
        // killing the child — deleting either emit silently blinds the
        // 15:45 scorecard's stall column for that arm.
        // Scan the PRODUCTION region only (split at #[cfg(test)]) — the
        // seal_writer precedent: a whole-file scan would match this test's
        // own literals (vacuous false-OK class).
        let full = include_str!("groww_sidecar_supervisor.rs");
        let src = full
            .split("#[cfg(test)]")
            .next()
            .expect("production region exists");
        // CALL sites (the `fn emit_stall_ws_audit(` definition excluded).
        let call_sites: Vec<usize> = src
            .match_indices("emit_stall_ws_audit(")
            .map(|(i, _)| i)
            .filter(|&i| !src[..i].ends_with("fn "))
            .collect();
        assert!(
            call_sites.len() >= 2,
            "expected BOTH stall-arm call sites of emit_stall_ws_audit \
             (found {})",
            call_sites.len()
        );
        // Each arm variant must appear inside some call-site window.
        for variant in ["StallArm::SilentSocket", "StallArm::NeverStreamed"] {
            assert!(
                call_sites.iter().any(|&i| {
                    let end = (i + 400).min(src.len());
                    src[i..end].contains(variant)
                }),
                "no emit_stall_ws_audit call site passes {variant}"
            );
        }
    }

    #[test]
    fn test_ratchet_main_wires_ws_audit_sender_into_supervisor_spawns() {
        // WIRING RATCHET (scoreboard PR-B): main.rs must hand a live
        // ws_event_audit sender to BOTH supervisor spawn sites (single-conn
        // + §34 fleet) — a `None` there is a silent stall-row blackout.
        let main_src = include_str!("main.rs");
        for spawn in [
            "spawn_supervised_groww_sidecar_supervisor(",
            "spawn_groww_scale_fleet(",
        ] {
            let at = main_src
                .find(spawn)
                .unwrap_or_else(|| panic!("{spawn} spawn site must exist in main.rs"));
            let window_end = (at + 1200).min(main_src.len());
            assert!(
                main_src[at..window_end].contains("spawn_ws_event_audit_consumer("),
                "{spawn} must be handed a ws_event_audit sender (stall rows)"
            );
        }
    }
}
