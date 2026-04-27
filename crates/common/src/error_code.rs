//! Central structured error-code taxonomy.
//!
//! Every known error/invariant in the tickvault workspace MUST appear as a
//! variant of [`ErrorCode`]. This is the single source of truth that:
//!
//! 1. Lets `error!` sites carry a stable, machine-parseable `code` field —
//!    Claude Code and Loki alert rules pattern-match on this.
//! 2. Maps every code to a [`Severity`] so the notification service can
//!    choose Telegram-only vs Telegram+SNS-SMS automatically.
//! 3. Maps every code to a runbook URL so operators and Claude sessions can
//!    jump directly to the authoritative enforcement rule.
//! 4. Lets the integration test
//!    `tests/error_code_rule_file_crossref.rs` assert that every variant
//!    is mentioned in at least one `.claude/rules/*.md` file AND that every
//!    rule code mentioned there has a matching variant here — the tree
//!    stays in sync mechanically.
//!
//! Phase 1 of `.claude/plans/active-plan.md`.

use std::fmt;
use std::str::FromStr;

/// Operational severity of an error event.
///
/// Order matters: `Info < Low < Medium < High < Critical`. The notification
/// service routes High/Critical events to both Telegram and AWS SNS SMS;
/// lower severities go Telegram-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Informational — no action required, visibility only.
    Info,
    /// Low — log + dashboard; no paging.
    Low,
    /// Medium — Telegram alert, best-effort auto-triage.
    Medium,
    /// High — Telegram + SNS SMS, manual triage likely.
    High,
    /// Critical — Telegram + SNS SMS, halts a sub-system, manual triage required.
    Critical,
}

impl Severity {
    /// Returns the canonical lower-cased label ("info", "low", ...).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Every known error/invariant in the tickvault workspace.
///
/// Variants are grouped by domain. New codes MUST be added here AND in the
/// corresponding `.claude/rules/` markdown file — the cross-ref integration
/// test enforces this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    // -----------------------------------------------------------------------
    // Instrument — Priority 0 (data-loss / correctness)
    // -----------------------------------------------------------------------
    /// Duplicate security_id rejected at CSV parse.
    InstrumentP0DuplicateSecurityId,
    /// Derivative count below minimum threshold post-build.
    InstrumentP0CountConsistency,
    /// Expired contract reached OMS submit Gate 4.
    InstrumentP0ExpiryAtGate4,
    /// Cache directory on tmpfs — not persistent.
    InstrumentP0CachePersistence,
    /// S3 remote backup configuration invalid.
    InstrumentP0S3Backup,
    /// Emergency download override triggered during market hours.
    InstrumentP0EmergencyDownload,

    // -----------------------------------------------------------------------
    // Instrument — Priority 1
    // -----------------------------------------------------------------------
    /// Daily CSV refresh scheduler boundary violation.
    InstrumentP1DailyScheduler,
    /// Delta detector missed a mutable field.
    InstrumentP1DeltaFieldCoverage,
    /// security_id reused across underlyings (add + expire collision).
    InstrumentP1SecurityIdReuse,
    /// Compound DEDUP key missing underlying_symbol.
    InstrumentP1CompoundDedupKey,
    /// Tick DEDUP key missing segment.
    InstrumentP1SegmentInTickDedup,
    /// Single-row-per-instrument lifecycle violated (row accumulation).
    InstrumentP1SingleRowLifecycle,
    /// Cross-segment security_id collision detected.
    InstrumentP1CrossSegmentCollision,

    // -----------------------------------------------------------------------
    // Instrument — Priority 2
    // -----------------------------------------------------------------------
    /// Trading-day guard triggered on weekend/holiday download.
    InstrumentP2TradingDayGuard,

    // -----------------------------------------------------------------------
    // Networking / API security
    // -----------------------------------------------------------------------
    /// IP monitor detected unexpected public IP change.
    GapNetIpMonitor,
    /// API auth middleware rejected request.
    GapSecApiAuth,

    // -----------------------------------------------------------------------
    // Order Management System
    // -----------------------------------------------------------------------
    /// OMS order lifecycle state-machine invalid transition.
    OmsGapStateMachine,
    /// OMS reconciliation mismatch between local and Dhan.
    OmsGapReconciliation,
    /// OMS circuit breaker tripped.
    OmsGapCircuitBreaker,
    /// OMS rate limit (SEBI 10/sec or daily cap) exceeded.
    OmsGapRateLimit,
    /// OMS idempotency tracking failed.
    OmsGapIdempotency,
    /// OMS dry-run safety gate triggered (unexpected HTTP attempt).
    OmsGapDryRunSafety,

    // -----------------------------------------------------------------------
    // WebSocket
    // -----------------------------------------------------------------------
    /// WebSocket disconnect code outside known set.
    WsGapDisconnectClassification,
    /// WebSocket subscription batching exceeded Dhan limits.
    WsGapSubscriptionBatching,
    /// WebSocket connection state-machine invalid transition.
    WsGapConnectionState,

    // -----------------------------------------------------------------------
    // Risk engine
    // -----------------------------------------------------------------------
    /// Pre-trade risk check rejected an order.
    RiskGapPreTrade,
    /// Position or P&L tracking invariant violated.
    RiskGapPositionPnl,
    /// Tick-gap detector fired above ERROR threshold.
    RiskGapTickGap,

    // -----------------------------------------------------------------------
    // Authentication
    // -----------------------------------------------------------------------
    /// Token expiry validation failed (expired or invalid state).
    AuthGapTokenExpiry,
    /// Only disconnect-code 807 should trigger token refresh; other code did.
    AuthGapDisconnectTokenMap,

    // -----------------------------------------------------------------------
    // Storage
    // -----------------------------------------------------------------------
    /// Tick DEDUP key constant missing segment column.
    StorageGapTickDedupSegment,
    /// f32→f64 conversion used raw widening (precision loss).
    StorageGapF32F64Precision,

    // -----------------------------------------------------------------------
    // Wave 1 hot-path / Phase 2 / prev-close / movers (PR #393)
    // -----------------------------------------------------------------------
    /// HOT-PATH-01: sync filesystem I/O failed inside an async writer task.
    HotPath01SyncFsFailed,
    /// HOT-PATH-02: hot-path writer queue full / closed / uninitialized.
    HotPath02WriterQueueDrop,
    /// PHASE2-01: Phase 2 dispatch failed (Failed event emitted).
    Phase201DispatchFailed,
    /// PHASE2-02: Phase2EmitGuard dropped without firing an outcome.
    Phase202EmitGuardDropped,
    /// PREVCLOSE-01: previous_close ILP append or flush failed.
    PrevClose01IlpFailed,
    /// PREVCLOSE-02: first_seen_set inconsistency (reserved for future use).
    PrevClose02FirstSeenInconsistency,
    /// MOVERS-01: stock movers persistence failed.
    Movers01StockPersistFailed,
    /// MOVERS-02: option movers persistence failed.
    Movers02OptionPersistFailed,

    // -----------------------------------------------------------------------
    // Dhan Trading API (DH-9xx)
    // -----------------------------------------------------------------------
    /// DH-901: Invalid auth — rotate token, retry once.
    Dh901InvalidAuth,
    /// DH-902: No API access — HALT + alert.
    Dh902NoApiAccess,
    /// DH-903: Account issue — HALT + alert.
    Dh903AccountIssue,
    /// DH-904: Rate limit — exponential backoff.
    Dh904RateLimit,
    /// DH-905: Input exception — never retry.
    Dh905InputException,
    /// DH-906: Order error — never retry.
    Dh906OrderError,
    /// DH-907: Data error — check params.
    Dh907DataError,
    /// DH-908: Internal server error — retry with backoff.
    Dh908InternalServerError,
    /// DH-909: Network error — retry with backoff.
    Dh909NetworkError,
    /// DH-910: Other Dhan error — log + alert.
    Dh910Other,

    // -----------------------------------------------------------------------
    // Dhan Data API (8xx)
    // -----------------------------------------------------------------------
    /// 800: Data API internal server error.
    Data800InternalServerError,
    /// 804: Instruments exceed per-connection limit.
    Data804InstrumentsExceedLimit,
    /// 805: Too many requests/connections — STOP ALL 60s.
    Data805TooManyConnections,
    /// 806: Data APIs not subscribed.
    Data806NotSubscribed,
    /// 807: Access token expired — trigger refresh.
    Data807TokenExpired,
    /// 808: Authentication failed.
    Data808AuthFailed,
    /// 809: Access token invalid.
    Data809TokenInvalid,
    /// 810: Client ID invalid.
    Data810ClientIdInvalid,
    /// 811: Invalid expiry date.
    Data811InvalidExpiry,
    /// 812: Invalid date format.
    Data812InvalidDateFormat,
    /// 813: Invalid SecurityId.
    Data813InvalidSecurityId,
    /// 814: Invalid request.
    Data814InvalidRequest,
}

impl ErrorCode {
    /// Returns the canonical code string used in logs + rule documentation.
    ///
    /// The strings here must match the codes used in `.claude/rules/*.md`
    /// verbatim — the cross-ref integration test enforces this.
    #[must_use]
    pub const fn code_str(self) -> &'static str {
        match self {
            // Instrument P0
            Self::InstrumentP0DuplicateSecurityId => "I-P0-01",
            Self::InstrumentP0CountConsistency => "I-P0-02",
            Self::InstrumentP0ExpiryAtGate4 => "I-P0-03",
            Self::InstrumentP0CachePersistence => "I-P0-04",
            Self::InstrumentP0S3Backup => "I-P0-05",
            Self::InstrumentP0EmergencyDownload => "I-P0-06",
            // Instrument P1
            Self::InstrumentP1DailyScheduler => "I-P1-01",
            Self::InstrumentP1DeltaFieldCoverage => "I-P1-02",
            Self::InstrumentP1SecurityIdReuse => "I-P1-03",
            Self::InstrumentP1CompoundDedupKey => "I-P1-05",
            Self::InstrumentP1SegmentInTickDedup => "I-P1-06",
            Self::InstrumentP1SingleRowLifecycle => "I-P1-08",
            Self::InstrumentP1CrossSegmentCollision => "I-P1-11",
            // Instrument P2
            Self::InstrumentP2TradingDayGuard => "I-P2-02",
            // GAP-NET / GAP-SEC
            Self::GapNetIpMonitor => "GAP-NET-01",
            Self::GapSecApiAuth => "GAP-SEC-01",
            // OMS
            Self::OmsGapStateMachine => "OMS-GAP-01",
            Self::OmsGapReconciliation => "OMS-GAP-02",
            Self::OmsGapCircuitBreaker => "OMS-GAP-03",
            Self::OmsGapRateLimit => "OMS-GAP-04",
            Self::OmsGapIdempotency => "OMS-GAP-05",
            Self::OmsGapDryRunSafety => "OMS-GAP-06",
            // WebSocket
            Self::WsGapDisconnectClassification => "WS-GAP-01",
            Self::WsGapSubscriptionBatching => "WS-GAP-02",
            Self::WsGapConnectionState => "WS-GAP-03",
            // Risk
            Self::RiskGapPreTrade => "RISK-GAP-01",
            Self::RiskGapPositionPnl => "RISK-GAP-02",
            Self::RiskGapTickGap => "RISK-GAP-03",
            // Auth
            Self::AuthGapTokenExpiry => "AUTH-GAP-01",
            Self::AuthGapDisconnectTokenMap => "AUTH-GAP-02",
            // Storage
            Self::StorageGapTickDedupSegment => "STORAGE-GAP-01",
            Self::StorageGapF32F64Precision => "STORAGE-GAP-02",
            // Wave 1 (PR #393)
            Self::HotPath01SyncFsFailed => "HOT-PATH-01",
            Self::HotPath02WriterQueueDrop => "HOT-PATH-02",
            Self::Phase201DispatchFailed => "PHASE2-01",
            Self::Phase202EmitGuardDropped => "PHASE2-02",
            Self::PrevClose01IlpFailed => "PREVCLOSE-01",
            Self::PrevClose02FirstSeenInconsistency => "PREVCLOSE-02",
            Self::Movers01StockPersistFailed => "MOVERS-01",
            Self::Movers02OptionPersistFailed => "MOVERS-02",
            // Dhan Trading API
            Self::Dh901InvalidAuth => "DH-901",
            Self::Dh902NoApiAccess => "DH-902",
            Self::Dh903AccountIssue => "DH-903",
            Self::Dh904RateLimit => "DH-904",
            Self::Dh905InputException => "DH-905",
            Self::Dh906OrderError => "DH-906",
            Self::Dh907DataError => "DH-907",
            Self::Dh908InternalServerError => "DH-908",
            Self::Dh909NetworkError => "DH-909",
            Self::Dh910Other => "DH-910",
            // Data API
            Self::Data800InternalServerError => "DATA-800",
            Self::Data804InstrumentsExceedLimit => "DATA-804",
            Self::Data805TooManyConnections => "DATA-805",
            Self::Data806NotSubscribed => "DATA-806",
            Self::Data807TokenExpired => "DATA-807",
            Self::Data808AuthFailed => "DATA-808",
            Self::Data809TokenInvalid => "DATA-809",
            Self::Data810ClientIdInvalid => "DATA-810",
            Self::Data811InvalidExpiry => "DATA-811",
            Self::Data812InvalidDateFormat => "DATA-812",
            Self::Data813InvalidSecurityId => "DATA-813",
            Self::Data814InvalidRequest => "DATA-814",
        }
    }

    /// Returns the severity this code should carry when emitted.
    #[must_use]
    pub const fn severity(self) -> Severity {
        match self {
            // Critical: auth / account / global connection cap
            Self::Dh901InvalidAuth
            | Self::Dh902NoApiAccess
            | Self::Dh903AccountIssue
            | Self::Data805TooManyConnections
            | Self::Data808AuthFailed
            | Self::Data809TokenInvalid
            | Self::Data810ClientIdInvalid
            | Self::AuthGapTokenExpiry
            | Self::AuthGapDisconnectTokenMap
            | Self::OmsGapCircuitBreaker
            | Self::OmsGapDryRunSafety
            | Self::InstrumentP0EmergencyDownload => Severity::Critical,
            // High: regulatory / order / risk / rate-limit
            Self::Dh904RateLimit
            | Self::Dh905InputException
            | Self::Dh906OrderError
            | Self::OmsGapStateMachine
            | Self::OmsGapReconciliation
            | Self::OmsGapRateLimit
            | Self::OmsGapIdempotency
            | Self::RiskGapPreTrade
            | Self::RiskGapPositionPnl
            | Self::InstrumentP0ExpiryAtGate4
            | Self::Data807TokenExpired
            | Self::Phase202EmitGuardDropped => Severity::High,
            // Medium: data pipeline correctness
            Self::InstrumentP0DuplicateSecurityId
            | Self::InstrumentP0CountConsistency
            | Self::InstrumentP0CachePersistence
            | Self::InstrumentP0S3Backup
            | Self::InstrumentP1SecurityIdReuse
            | Self::InstrumentP1CrossSegmentCollision
            | Self::InstrumentP1SegmentInTickDedup
            | Self::InstrumentP1CompoundDedupKey
            | Self::InstrumentP1SingleRowLifecycle
            | Self::WsGapDisconnectClassification
            | Self::WsGapSubscriptionBatching
            | Self::WsGapConnectionState
            | Self::RiskGapTickGap
            | Self::StorageGapTickDedupSegment
            | Self::StorageGapF32F64Precision
            | Self::GapNetIpMonitor
            | Self::GapSecApiAuth
            | Self::Dh907DataError
            | Self::Dh908InternalServerError
            | Self::Dh909NetworkError
            | Self::Data800InternalServerError
            | Self::Data804InstrumentsExceedLimit
            | Self::Data806NotSubscribed
            | Self::Data811InvalidExpiry
            | Self::Data812InvalidDateFormat
            | Self::Data813InvalidSecurityId
            | Self::Data814InvalidRequest
            | Self::HotPath01SyncFsFailed
            | Self::Phase201DispatchFailed
            | Self::PrevClose01IlpFailed
            | Self::PrevClose02FirstSeenInconsistency
            | Self::Movers01StockPersistFailed
            | Self::Movers02OptionPersistFailed => Severity::Medium,
            // Low: scheduler / field coverage / trading-day / Dhan other
            Self::InstrumentP1DailyScheduler
            | Self::InstrumentP1DeltaFieldCoverage
            | Self::InstrumentP2TradingDayGuard
            | Self::Dh910Other
            | Self::HotPath02WriterQueueDrop => Severity::Low,
        }
    }

    /// Returns the canonical runbook path inside the repo (relative) that
    /// documents how to triage this error.
    ///
    /// Returning a path rather than a URL makes this usable by Claude Code
    /// sessions that don't necessarily have HTTP egress.
    #[must_use]
    pub const fn runbook_path(self) -> &'static str {
        match self {
            Self::InstrumentP0DuplicateSecurityId
            | Self::InstrumentP0CountConsistency
            | Self::InstrumentP0ExpiryAtGate4
            | Self::InstrumentP0CachePersistence
            | Self::InstrumentP0S3Backup
            | Self::InstrumentP0EmergencyDownload
            | Self::InstrumentP1DailyScheduler
            | Self::InstrumentP1DeltaFieldCoverage
            | Self::InstrumentP1SecurityIdReuse
            | Self::InstrumentP1CompoundDedupKey
            | Self::InstrumentP1SegmentInTickDedup
            | Self::InstrumentP1SingleRowLifecycle
            | Self::InstrumentP1CrossSegmentCollision
            | Self::InstrumentP2TradingDayGuard
            | Self::GapNetIpMonitor
            | Self::GapSecApiAuth
            | Self::OmsGapStateMachine
            | Self::OmsGapReconciliation
            | Self::OmsGapCircuitBreaker
            | Self::OmsGapRateLimit
            | Self::OmsGapIdempotency
            | Self::OmsGapDryRunSafety
            | Self::WsGapDisconnectClassification
            | Self::WsGapSubscriptionBatching
            | Self::WsGapConnectionState
            | Self::RiskGapPreTrade
            | Self::RiskGapPositionPnl
            | Self::RiskGapTickGap
            | Self::AuthGapTokenExpiry
            | Self::AuthGapDisconnectTokenMap
            | Self::StorageGapTickDedupSegment
            | Self::StorageGapF32F64Precision => ".claude/rules/project/gap-enforcement.md",
            Self::HotPath01SyncFsFailed
            | Self::HotPath02WriterQueueDrop
            | Self::Phase201DispatchFailed
            | Self::Phase202EmitGuardDropped
            | Self::PrevClose01IlpFailed
            | Self::PrevClose02FirstSeenInconsistency
            | Self::Movers01StockPersistFailed
            | Self::Movers02OptionPersistFailed => ".claude/rules/project/wave-1-error-codes.md",
            Self::Dh901InvalidAuth
            | Self::Dh902NoApiAccess
            | Self::Dh903AccountIssue
            | Self::Dh904RateLimit
            | Self::Dh905InputException
            | Self::Dh906OrderError
            | Self::Dh907DataError
            | Self::Dh908InternalServerError
            | Self::Dh909NetworkError
            | Self::Dh910Other => ".claude/rules/dhan/annexure-enums.md",
            Self::Data800InternalServerError
            | Self::Data804InstrumentsExceedLimit
            | Self::Data805TooManyConnections
            | Self::Data806NotSubscribed
            | Self::Data807TokenExpired
            | Self::Data808AuthFailed
            | Self::Data809TokenInvalid
            | Self::Data810ClientIdInvalid
            | Self::Data811InvalidExpiry
            | Self::Data812InvalidDateFormat
            | Self::Data813InvalidSecurityId
            | Self::Data814InvalidRequest => ".claude/rules/dhan/annexure-enums.md",
        }
    }

    /// True when Claude Code auto-triage should attempt a fix before
    /// escalating. False means operator action is required — the auto-triage
    /// daemon will log a summary and stop.
    ///
    /// Conservative default: Critical errors are NEVER auto-actioned.
    #[must_use]
    pub const fn is_auto_triage_safe(self) -> bool {
        !matches!(self.severity(), Severity::Critical)
    }

    /// All known variants, in declaration order.
    ///
    /// Used by the cross-ref integration test and by the future error-code
    /// catalogue dashboard. Update this list whenever a new variant is added —
    /// the `test_all_variants_list_is_exhaustive` unit test will fail
    /// otherwise (compile-time via non_exhaustive pattern match).
    #[must_use]
    pub fn all() -> &'static [ErrorCode] {
        &[
            Self::InstrumentP0DuplicateSecurityId,
            Self::InstrumentP0CountConsistency,
            Self::InstrumentP0ExpiryAtGate4,
            Self::InstrumentP0CachePersistence,
            Self::InstrumentP0S3Backup,
            Self::InstrumentP0EmergencyDownload,
            Self::InstrumentP1DailyScheduler,
            Self::InstrumentP1DeltaFieldCoverage,
            Self::InstrumentP1SecurityIdReuse,
            Self::InstrumentP1CompoundDedupKey,
            Self::InstrumentP1SegmentInTickDedup,
            Self::InstrumentP1SingleRowLifecycle,
            Self::InstrumentP1CrossSegmentCollision,
            Self::InstrumentP2TradingDayGuard,
            Self::GapNetIpMonitor,
            Self::GapSecApiAuth,
            Self::OmsGapStateMachine,
            Self::OmsGapReconciliation,
            Self::OmsGapCircuitBreaker,
            Self::OmsGapRateLimit,
            Self::OmsGapIdempotency,
            Self::OmsGapDryRunSafety,
            Self::WsGapDisconnectClassification,
            Self::WsGapSubscriptionBatching,
            Self::WsGapConnectionState,
            Self::RiskGapPreTrade,
            Self::RiskGapPositionPnl,
            Self::RiskGapTickGap,
            Self::AuthGapTokenExpiry,
            Self::AuthGapDisconnectTokenMap,
            Self::StorageGapTickDedupSegment,
            Self::StorageGapF32F64Precision,
            Self::Dh901InvalidAuth,
            Self::Dh902NoApiAccess,
            Self::Dh903AccountIssue,
            Self::Dh904RateLimit,
            Self::Dh905InputException,
            Self::Dh906OrderError,
            Self::Dh907DataError,
            Self::Dh908InternalServerError,
            Self::Dh909NetworkError,
            Self::Dh910Other,
            Self::Data800InternalServerError,
            Self::Data804InstrumentsExceedLimit,
            Self::Data805TooManyConnections,
            Self::Data806NotSubscribed,
            Self::Data807TokenExpired,
            Self::Data808AuthFailed,
            Self::Data809TokenInvalid,
            Self::Data810ClientIdInvalid,
            Self::Data811InvalidExpiry,
            Self::Data812InvalidDateFormat,
            Self::Data813InvalidSecurityId,
            Self::Data814InvalidRequest,
            Self::HotPath01SyncFsFailed,
            Self::HotPath02WriterQueueDrop,
            Self::Phase201DispatchFailed,
            Self::Phase202EmitGuardDropped,
            Self::PrevClose01IlpFailed,
            Self::PrevClose02FirstSeenInconsistency,
            Self::Movers01StockPersistFailed,
            Self::Movers02OptionPersistFailed,
        ]
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code_str())
    }
}

/// Error returned when parsing an unknown code string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownErrorCode(pub String);

impl fmt::Display for UnknownErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown error code: {}", self.0)
    }
}

impl std::error::Error for UnknownErrorCode {}

impl FromStr for ErrorCode {
    type Err = UnknownErrorCode;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for code in Self::all() {
            if code.code_str() == s {
                return Ok(*code);
            }
        }
        Err(UnknownErrorCode(s.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Unit tests — invariants that must hold at all times
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_all_variants_have_unique_code_str() {
        let mut seen: HashSet<&'static str> = HashSet::new();
        for code in ErrorCode::all() {
            assert!(
                seen.insert(code.code_str()),
                "duplicate code_str: {}",
                code.code_str()
            );
        }
    }

    #[test]
    fn test_code_str_roundtrip_via_from_str() {
        for code in ErrorCode::all() {
            let parsed: ErrorCode = code.code_str().parse().unwrap_or_else(|e| {
                panic!("FromStr failed to roundtrip for {}: {e:?}", code.code_str())
            });
            assert_eq!(parsed, *code);
        }
    }

    #[test]
    fn test_from_str_rejects_unknown_code() {
        let result: Result<ErrorCode, _> = "BOGUS-999".parse();
        assert!(result.is_err());
    }

    #[test]
    fn test_every_variant_has_non_empty_runbook_path() {
        for code in ErrorCode::all() {
            let path = code.runbook_path();
            assert!(
                !path.is_empty(),
                "runbook_path empty for {}",
                code.code_str()
            );
            assert!(
                path.starts_with(".claude/"),
                "runbook_path for {} should point at .claude/: got {}",
                code.code_str(),
                path
            );
        }
    }

    #[test]
    fn test_severity_ordering() {
        assert!(Severity::Info < Severity::Low);
        assert!(Severity::Low < Severity::Medium);
        assert!(Severity::Medium < Severity::High);
        assert!(Severity::High < Severity::Critical);
    }

    #[test]
    fn test_severity_as_str_is_stable() {
        assert_eq!(Severity::Info.as_str(), "info");
        assert_eq!(Severity::Low.as_str(), "low");
        assert_eq!(Severity::Medium.as_str(), "medium");
        assert_eq!(Severity::High.as_str(), "high");
        assert_eq!(Severity::Critical.as_str(), "critical");
    }

    #[test]
    fn test_critical_codes_never_auto_triage() {
        for code in ErrorCode::all() {
            if code.severity() == Severity::Critical {
                assert!(
                    !code.is_auto_triage_safe(),
                    "{} is Critical but is_auto_triage_safe returned true",
                    code.code_str()
                );
            }
        }
    }

    #[test]
    fn test_every_severity_is_assigned_to_at_least_one_code() {
        let mut seen = HashSet::new();
        for code in ErrorCode::all() {
            seen.insert(code.severity());
        }
        // Info is allowed to be unused (no code is purely informational in
        // the current catalogue); the remaining four must all appear.
        assert!(seen.contains(&Severity::Low));
        assert!(seen.contains(&Severity::Medium));
        assert!(seen.contains(&Severity::High));
        assert!(seen.contains(&Severity::Critical));
    }

    #[test]
    fn test_display_matches_code_str() {
        for code in ErrorCode::all() {
            assert_eq!(code.to_string(), code.code_str());
        }
    }

    #[test]
    fn test_all_list_length_matches_catalogue_size() {
        // If this fails, the `all()` list was not updated when a new variant
        // was added. Keep this count in sync with the enum.
        // 2026-04-27 (Wave 1): bumped 54 -> 62 for 8 new variants
        // (HOT-PATH-01/02, PHASE2-01/02, PREVCLOSE-01/02, MOVERS-01/02).
        assert_eq!(ErrorCode::all().len(), 62);
    }

    #[test]
    fn test_code_str_follows_expected_prefix_pattern() {
        for code in ErrorCode::all() {
            let s = code.code_str();
            let has_known_prefix = s.starts_with("I-P")
                || s.starts_with("GAP-")
                || s.starts_with("OMS-GAP-")
                || s.starts_with("WS-GAP-")
                || s.starts_with("RISK-GAP-")
                || s.starts_with("AUTH-GAP-")
                || s.starts_with("STORAGE-GAP-")
                || s.starts_with("DH-")
                || s.starts_with("DATA-")
                // Wave 1 (PR #393): hot-path / phase2 / prev-close / movers prefixes
                || s.starts_with("HOT-PATH-")
                || s.starts_with("PHASE2-")
                || s.starts_with("PREVCLOSE-")
                || s.starts_with("MOVERS-");
            assert!(has_known_prefix, "unexpected code prefix: {s}");
        }
    }

    #[test]
    fn test_severity_display_impl_matches_as_str() {
        // Covers `impl fmt::Display for Severity` (error_code.rs:56-60).
        for sev in [
            Severity::Info,
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ] {
            assert_eq!(format!("{sev}"), sev.as_str());
        }
    }

    #[test]
    fn test_unknown_error_code_display_and_error_trait() {
        // Covers `impl fmt::Display for UnknownErrorCode`
        // (error_code.rs:516-520) and the std::error::Error impl blanket use.
        let err = UnknownErrorCode("BOGUS-999".to_string());
        let rendered = format!("{err}");
        assert_eq!(rendered, "unknown error code: BOGUS-999");
        // Exercises std::error::Error path via the trait object.
        let as_err: &dyn std::error::Error = &err;
        assert_eq!(as_err.to_string(), "unknown error code: BOGUS-999");
    }

    #[test]
    fn test_unknown_error_code_equality_and_clone() {
        // Covers PartialEq + Clone auto-derives so they are exercised.
        let a = UnknownErrorCode("X".to_string());
        let b = a.clone();
        assert_eq!(a, b);
    }
}
