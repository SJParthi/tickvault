//! Dual-feed scoreboard **blame classifier** — the single, pure, TOTAL
//! attribution function for every feed episode (disconnect / stall /
//! process-death) persisted to `feed_episode_audit`.
//!
//! Operator directive 2026-07-10 (dual-feed scoreboard): *"ensure and CAPTURE
//! that the issue really arose from the broker side"*. Every episode row MUST
//! carry a blame verdict — broker / ours / indeterminate — and the verdict
//! must come from ONE code path so the month-end tally is consistent.
//!
//! # Honest envelope (no hallucination)
//! Blame is EVIDENTIAL, not proof. Per `disconnect_cause.rs`'s own envelope a
//! lone mid-stream TCP reset is NOT attributable from one event — those rows
//! are honestly `Indeterminate` and the month-end runbook
//! (`docs/runbooks/dual-feed-scoreboard.md`) requires operator review of every
//! indeterminate before signing a verdict. The classifier upgrades a reset to
//! `Broker` ONLY on high-precision same-day corroboration (a WS-GAP-09
//! `bare_dhan_reset` / `in_window_429_ride_out` line within ±120s).
//!
//! # Totality (the no-blank-blame ratchet, layer 1)
//! [`classify_episode`] is a TOTAL function: every input — including unknown
//! future source strings, junk episode kinds, garbage codes — yields a
//! [`BlameClass`] + a non-empty reason slug. There is no `Option`, no panic
//! path, and the fail-closed floor is `Indeterminate` / `unclassified`.
//! Pinned by an exhaustive table test + a proptest over arbitrary inputs.
//!
//! # Performance
//! Pure `Copy`/`&str` matching, zero allocation, cold path (episodes are
//! rare; classification runs once per episode at the 15:45 IST aggregation
//! and once per synthesized process-death at boot). O(1) per call — a fixed
//! number of comparisons over short strings.

/// The blame verdict persisted to `feed_episode_audit.blame`.
///
/// Deliberately 3 variants and NOT `Option<..>` — the episode writer takes
/// `BlameClass` by value, so persisting an episode WITHOUT a blame class is a
/// compile error (no-blank-blame ratchet, layer 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlameClass {
    /// The broker (Dhan / Groww server side) caused the episode.
    Broker,
    /// Our side caused it (process death, feed toggle, dual-instance,
    /// resource pressure, stale shared token).
    Ours,
    /// Cannot be attributed from the available evidence — honest bucket,
    /// reviewed by the operator at month end.
    Indeterminate,
}

impl BlameClass {
    /// Stable wire label for the `blame` SYMBOL column. Never empty.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Broker => "broker",
            Self::Ours => "ours",
            Self::Indeterminate => "indeterminate",
        }
    }

    /// All variants — lets tests assert label uniqueness without drifting.
    #[must_use]
    pub const fn all() -> [BlameClass; 3] {
        [Self::Broker, Self::Ours, Self::Indeterminate]
    }
}

/// Episode-kind wire labels the classifier understands (the
/// `feed_episode_audit.episode_kind` SYMBOL values). Unknown kinds still
/// classify (total fn) — they land `Indeterminate`/`unclassified`.
pub const EPISODE_KIND_DISCONNECT: &str = "disconnect";
/// Off-hours disconnect (Dhan pre/post-market idle cleanup class).
pub const EPISODE_KIND_OFF_HOURS_DISCONNECT: &str = "off_hours_disconnect";
/// Sidecar stall restart (FEED-STALL-01 semantics). Emitter lands in PR-2;
/// the classifier arm exists day-1 so PR-2 activates it with zero edits here.
pub const EPISODE_KIND_STALL_RESTART: &str = "stall_restart";
/// Never-streamed restart (FEED-STALL-01 §1b semantics). Emitter in PR-2.
pub const EPISODE_KIND_NEVER_STREAMED_RESTART: &str = "never_streamed_restart";
/// Boot-reconciled process death (the dying process wrote no disconnect row).
pub const EPISODE_KIND_PROCESS_DEATH: &str = "process_death";

/// Everything the classifier may consider for one episode. All fields are
/// plain evidence — the classifier itself does NO I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpisodeEvidence<'a> {
    /// Episode kind wire label (see the `EPISODE_KIND_*` consts).
    pub episode_kind: &'a str,
    /// Feed wire label (`"dhan"` / `"groww"` / anything — total).
    pub feed: &'a str,
    /// The `ws_event_audit.source` label verbatim (Dhan rows carry the
    /// human `disconnect_cause::label()` strings; Groww rows carry machine
    /// slugs like `"feed_disabled"`).
    pub source: &'a str,
    /// Dhan disconnect code from the audit row; `-1` sentinel = none.
    pub dhan_code: i64,
    /// A WS-GAP-09 `bare_dhan_reset` line within ±120s of the episode
    /// (same-day errors.jsonl correlation scan).
    pub ws_gap9_bare_reset_overlap: bool,
    /// A WS-GAP-09 `in_window_429_ride_out` line within ±120s.
    pub ws_gap9_429_overlap: bool,
    /// A RESILIENCE-01/03 (dual-instance) line the SAME trading day.
    pub resilience_peer_evidence: bool,
    /// A PROC-01 / RESOURCE-01..03 line within ±300s of the episode.
    pub resource_pressure_overlap: bool,
    /// Stall classification slug for stall episodes (`""` when N/A). PR-2
    /// supplies the FIXED `classify_sidecar_line` slugs — never raw text.
    pub stall_reason: &'a str,
    /// Process-death deploy-vs-crash evidence: `Some(true)` when the booting
    /// binary's git sha differs from the last-deployed sha (a deploy landed),
    /// `Some(false)` when identical, `None` when either side is unknown
    /// (SSM unreachable / `unknown` build) — fail-soft.
    pub build_sha_changed: Option<bool>,
}

impl<'a> EpisodeEvidence<'a> {
    /// Evidence with every corroboration input absent — the minimal shape for
    /// classifying a bare audit row.
    #[must_use]
    pub const fn bare(
        episode_kind: &'a str,
        feed: &'a str,
        source: &'a str,
        dhan_code: i64,
    ) -> Self {
        Self {
            episode_kind,
            feed,
            source,
            dhan_code,
            ws_gap9_bare_reset_overlap: false,
            ws_gap9_429_overlap: false,
            resilience_peer_evidence: false,
            resource_pressure_overlap: false,
            stall_reason: "",
            build_sha_changed: None,
        }
    }
}

/// Reason slug for the fail-closed floor (anything the table below does not
/// recognise). Pinned by the totality tests.
pub const BLAME_REASON_UNCLASSIFIED: &str = "unclassified";

/// Classify one episode. TOTAL — every input yields `(class, non-empty slug)`.
///
/// Attribution ladder (order encodes priority; each row from the contract's
/// §4 mapping table):
///
/// 1. `process_death` → **ours** (`deploy_restart` when the build sha
///    changed, else `process_restart`) — a death of OUR process is
///    definitionally ours, per the operator directive.
/// 2. Authoritative Dhan codes: 805 → ours `dual_instance` when a same-day
///    RESILIENCE-01/03 peer-lock line exists, else broker `rate_limit_805`;
///    807 → broker `auth_token_expired` (our renewal duty is tracked via
///    AUTH-GAP-*, but the broker delivered the reject); 806/808..814 →
///    broker `auth_entitlement`.
/// 3. Groww `feed_disabled` → ours `feed_toggle` (the operator/gate turned
///    the feed off).
/// 4. Stall episodes → broker `silent_socket` / `never_streamed` /
///    `entitlement_reject`, EXCEPT the token-stale class → ours
///    `token_minter_stale` (the shared-minter Lambda is our duty).
/// 5. Off-hours disconnects → indeterminate `off_hours_idle` (expected
///    pre/post-market cleanup noise; excluded from headline counts).
/// 6. Resource pressure (PROC-01 / RESOURCE-* within ±300s) → ours
///    `resource_pressure` — a reset while our box is starving is on us.
/// 7. Mid-stream resets (`Dhan or network`): broker `bare_rst` /
///    `rate_limit_429` ONLY with a ±120s WS-GAP-09 corroboration line;
///    otherwise indeterminate `transport_ambiguous` (the
///    `disconnect_cause.rs` honest envelope).
/// 8. Connect-phase failures (`Network / connection`) → indeterminate
///    `network_path`; `Unknown` → indeterminate `unknown_cause`.
/// 9. Everything else → indeterminate [`BLAME_REASON_UNCLASSIFIED`].
#[must_use]
pub fn classify_episode(e: &EpisodeEvidence<'_>) -> (BlameClass, &'static str) {
    // 1. Process death — always ours (the directive: deploy restarts and
    //    crashes are both our side; only the sub-reason differs).
    if e.episode_kind == EPISODE_KIND_PROCESS_DEATH {
        return match e.build_sha_changed {
            Some(true) => (BlameClass::Ours, "deploy_restart"),
            // Unchanged sha OR unknown (fail-soft) — crash / watchdog kill.
            Some(false) | None => (BlameClass::Ours, "process_restart"),
        };
    }

    // 2. Authoritative Dhan codes (from the audit row's dhan_code column,
    //    with the human source label as a code-less-row fallback — never
    //    scraped digit substrings, per the disconnect_cause 2026-06-12
    //    security finding).
    let is_805 = e.dhan_code == 805 || e.source == "Dhan (another login)";
    if is_805 {
        return if e.resilience_peer_evidence {
            (BlameClass::Ours, "dual_instance")
        } else {
            (BlameClass::Broker, "rate_limit_805")
        };
    }
    let is_807 = e.dhan_code == 807 || e.source == "Dhan (login token expired)";
    if is_807 {
        return (BlameClass::Broker, "auth_token_expired");
    }
    let is_auth_entitlement =
        matches!(e.dhan_code, 806 | 808..=814) || e.source == "Dhan (login / data plan)";
    if is_auth_entitlement {
        return (BlameClass::Broker, "auth_entitlement");
    }

    // 3. Machine slugs from OUR OWN lifecycle machinery (hostile review
    //    2026-07-10 — audited against every live emit site):
    //    - `feed_disabled` (Groww disable gate) → the toggle closed the
    //      socket; ours.
    //    - `bridge_died` (groww_bridge supervisor falling edge — OUR task
    //      panicked and was respawned, FEED-SUPERVISOR-01 class) →
    //      definitionally ours, never "unclear".
    if e.source == "feed_disabled" {
        return (BlameClass::Ours, "feed_toggle");
    }
    if e.source == "bridge_died" {
        return (BlameClass::Ours, "bridge_task_died");
    }

    // 4. Stall episodes (emitters land in PR-2; arms live day-1).
    if e.episode_kind == EPISODE_KIND_STALL_RESTART
        || e.episode_kind == EPISODE_KIND_NEVER_STREAMED_RESTART
    {
        // Authorization / Permissions / SILENT-FEED entitlement class —
        // checked BEFORE the auth/token class because "authorization"
        // contains the substring "auth" (a NATS-level reject is a broker
        // entitlement refusal, not our minter's duty).
        if e.stall_reason.contains("entitlement")
            || e.stall_reason.contains("authoriz")
            || e.stall_reason.contains("permission")
        {
            return (BlameClass::Broker, "entitlement_reject");
        }
        // Token-stale / auth class → the shared-minter Lambda duty (ours).
        if e.stall_reason.contains("auth") || e.stall_reason.contains("token") {
            return (BlameClass::Ours, "token_minter_stale");
        }
        if e.episode_kind == EPISODE_KIND_NEVER_STREAMED_RESTART
            || e.stall_reason.contains("never_streamed")
        {
            return (BlameClass::Broker, "never_streamed");
        }
        // Plain silent-socket stall (FEED-STALL-01 semantics: healthy host,
        // dead server-side socket).
        return (BlameClass::Broker, "silent_socket");
    }

    // 5. Off-hours disconnects — expected idle-cleanup noise; excluded from
    //    the headline market-hours count, honestly unattributed.
    if e.episode_kind == EPISODE_KIND_OFF_HOURS_DISCONNECT {
        return (BlameClass::Indeterminate, "off_hours_idle");
    }

    // 6. Resource pressure — our box was starving (fd / RSS / OOM / disk)
    //    within ±300s of the episode; the drop is on us.
    if e.resource_pressure_overlap {
        return (BlameClass::Ours, "resource_pressure");
    }

    // 7. Mid-stream reset: broker ONLY with WS-GAP-09 corroboration.
    if e.source == "Dhan or network" {
        if e.ws_gap9_429_overlap {
            return (BlameClass::Broker, "rate_limit_429");
        }
        if e.ws_gap9_bare_reset_overlap {
            return (BlameClass::Broker, "bare_rst");
        }
        return (BlameClass::Indeterminate, "transport_ambiguous");
    }

    // 8. Connect-phase / unknown buckets. `clean close` is the
    //    order-update WS clean-close arm (server Close frame / stream end —
    //    Dhan's OTHER documented auth-rejection delivery mode OR a benign
    //    idle-day close): not attributable from the row alone, but a NAMED
    //    slug beats the unclassified floor for month-end review.
    if e.source == "Network / connection" {
        return (BlameClass::Indeterminate, "network_path");
    }
    if e.source == "clean close" {
        return (BlameClass::Indeterminate, "clean_close");
    }
    if e.source == "Unknown" {
        return (BlameClass::Indeterminate, "unknown_cause");
    }

    // 9. The fail-closed floor — an unknown future source string / kind can
    //    never be blank, never panic, never silently blamed.
    (BlameClass::Indeterminate, BLAME_REASON_UNCLASSIFIED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_blame_class_labels_stable_and_nonempty() {
        assert_eq!(BlameClass::Broker.as_str(), "broker");
        assert_eq!(BlameClass::Ours.as_str(), "ours");
        assert_eq!(BlameClass::Indeterminate.as_str(), "indeterminate");
        let labels: Vec<&str> = BlameClass::all().iter().map(|b| b.as_str()).collect();
        let unique: std::collections::HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(unique.len(), labels.len(), "blame labels must be unique");
        for l in labels {
            assert!(!l.is_empty(), "blame label must never be empty");
        }
        assert_eq!(BlameClass::all().len(), 3);
    }

    #[test]
    fn test_classify_dhan_805_with_peer_evidence_is_ours_dual_instance() {
        let mut e =
            EpisodeEvidence::bare(EPISODE_KIND_DISCONNECT, "dhan", "Dhan (another login)", 805);
        e.resilience_peer_evidence = true;
        assert_eq!(classify_episode(&e), (BlameClass::Ours, "dual_instance"));
        // Code alone (source drifted) still classifies via the code.
        let mut by_code = EpisodeEvidence::bare(EPISODE_KIND_DISCONNECT, "dhan", "x", 805);
        by_code.resilience_peer_evidence = true;
        assert_eq!(
            classify_episode(&by_code),
            (BlameClass::Ours, "dual_instance")
        );
    }

    #[test]
    fn test_classify_dhan_805_without_evidence_is_broker() {
        // No peer-lock line that day (incl. the >48h backfill case where the
        // errors.jsonl evidence has aged out — runbook note): the
        // connection-cap kill is Dhan-side.
        let e = EpisodeEvidence::bare(EPISODE_KIND_DISCONNECT, "dhan", "Dhan (another login)", 805);
        assert_eq!(classify_episode(&e), (BlameClass::Broker, "rate_limit_805"));
    }

    #[test]
    fn test_classify_dhan_807_is_broker_auth_token_expired() {
        let e = EpisodeEvidence::bare(
            EPISODE_KIND_DISCONNECT,
            "dhan",
            "Dhan (login token expired)",
            807,
        );
        assert_eq!(
            classify_episode(&e),
            (BlameClass::Broker, "auth_token_expired")
        );
        // Source-only fallback (a -1 sentinel row with the human label).
        let by_source = EpisodeEvidence::bare(
            EPISODE_KIND_DISCONNECT,
            "dhan",
            "Dhan (login token expired)",
            -1,
        );
        assert_eq!(
            classify_episode(&by_source),
            (BlameClass::Broker, "auth_token_expired")
        );
    }

    #[test]
    fn test_classify_dhan_auth_entitlement_codes_are_broker() {
        for code in [806_i64, 808, 809, 810, 811, 812, 813, 814] {
            let e = EpisodeEvidence::bare(
                EPISODE_KIND_DISCONNECT,
                "dhan",
                "Dhan (login / data plan)",
                code,
            );
            assert_eq!(
                classify_episode(&e),
                (BlameClass::Broker, "auth_entitlement"),
                "code {code} must classify broker/auth_entitlement"
            );
        }
        // Label without a code (sentinel -1) still maps.
        let by_source = EpisodeEvidence::bare(
            EPISODE_KIND_DISCONNECT,
            "dhan",
            "Dhan (login / data plan)",
            -1,
        );
        assert_eq!(
            classify_episode(&by_source),
            (BlameClass::Broker, "auth_entitlement")
        );
    }

    #[test]
    fn test_classify_rst_with_ws_gap9_overlap_is_broker() {
        let mut bare =
            EpisodeEvidence::bare(EPISODE_KIND_DISCONNECT, "dhan", "Dhan or network", -1);
        bare.ws_gap9_bare_reset_overlap = true;
        assert_eq!(classify_episode(&bare), (BlameClass::Broker, "bare_rst"));

        let mut r429 =
            EpisodeEvidence::bare(EPISODE_KIND_DISCONNECT, "dhan", "Dhan or network", -1);
        r429.ws_gap9_429_overlap = true;
        assert_eq!(
            classify_episode(&r429),
            (BlameClass::Broker, "rate_limit_429")
        );

        // 429 corroboration outranks bare-reset when both overlap.
        let mut both = r429;
        both.ws_gap9_bare_reset_overlap = true;
        assert_eq!(
            classify_episode(&both),
            (BlameClass::Broker, "rate_limit_429")
        );
    }

    #[test]
    fn test_classify_rst_without_overlap_is_indeterminate() {
        // The disconnect_cause.rs honest envelope: a lone reset is NOT
        // attributable — never silently blamed on the broker.
        let e = EpisodeEvidence::bare(EPISODE_KIND_DISCONNECT, "dhan", "Dhan or network", -1);
        assert_eq!(
            classify_episode(&e),
            (BlameClass::Indeterminate, "transport_ambiguous")
        );
    }

    #[test]
    fn test_classify_network_and_unknown_are_indeterminate() {
        let net =
            EpisodeEvidence::bare(EPISODE_KIND_DISCONNECT, "dhan", "Network / connection", -1);
        assert_eq!(
            classify_episode(&net),
            (BlameClass::Indeterminate, "network_path")
        );
        let unk = EpisodeEvidence::bare(EPISODE_KIND_DISCONNECT, "dhan", "Unknown", -1);
        assert_eq!(
            classify_episode(&unk),
            (BlameClass::Indeterminate, "unknown_cause")
        );
    }

    #[test]
    fn test_classify_groww_feed_toggle_is_ours() {
        for kind in [EPISODE_KIND_DISCONNECT, EPISODE_KIND_OFF_HOURS_DISCONNECT] {
            let e = EpisodeEvidence::bare(kind, "groww", "feed_disabled", -1);
            assert_eq!(
                classify_episode(&e),
                (BlameClass::Ours, "feed_toggle"),
                "feed_disabled ({kind}) is our toggle, never broker"
            );
        }
    }

    #[test]
    fn test_classify_groww_bridge_died_is_ours() {
        // Hostile review 2026-07-10: the groww_bridge supervisor emits a
        // Disconnected row with source `bridge_died` when OUR task panics
        // and is respawned (FEED-SUPERVISOR-01 class) — definitionally
        // ours, never the unclassified/indeterminate floor.
        for kind in [EPISODE_KIND_DISCONNECT, EPISODE_KIND_OFF_HOURS_DISCONNECT] {
            let e = EpisodeEvidence::bare(kind, "groww", "bridge_died", -1);
            assert_eq!(
                classify_episode(&e),
                (BlameClass::Ours, "bridge_task_died"),
                "bridge_died ({kind}) is our own task death"
            );
        }
    }

    #[test]
    fn test_classify_order_update_clean_close_is_named_indeterminate() {
        // The order-update WS `clean close` slug: not attributable from the
        // row alone (idle-day close vs auth-reject delivery), but it gets a
        // NAMED slug for month-end review instead of the unclassified floor.
        let e = EpisodeEvidence::bare(EPISODE_KIND_DISCONNECT, "dhan", "clean close", -1);
        assert_eq!(
            classify_episode(&e),
            (BlameClass::Indeterminate, "clean_close")
        );
    }

    #[test]
    fn test_classify_stall_reasons() {
        // FEED-STALL-01 silent socket → broker.
        let stall = EpisodeEvidence {
            stall_reason: "stall",
            ..EpisodeEvidence::bare(EPISODE_KIND_STALL_RESTART, "groww", "stall_watchdog", -1)
        };
        assert_eq!(
            classify_episode(&stall),
            (BlameClass::Broker, "silent_socket")
        );
        // Never-streamed (kind OR reason slug) → broker.
        let ns_kind = EpisodeEvidence::bare(
            EPISODE_KIND_NEVER_STREAMED_RESTART,
            "groww",
            "stall_watchdog",
            -1,
        );
        assert_eq!(
            classify_episode(&ns_kind),
            (BlameClass::Broker, "never_streamed")
        );
        let ns_reason = EpisodeEvidence {
            stall_reason: "never_streamed",
            ..EpisodeEvidence::bare(EPISODE_KIND_STALL_RESTART, "groww", "stall_watchdog", -1)
        };
        assert_eq!(
            classify_episode(&ns_reason),
            (BlameClass::Broker, "never_streamed")
        );
        // FEED-REJECT-01 auth / token-stale class → the shared minter is OUR duty.
        for reason in ["error [auth]", "access token stale"] {
            let e = EpisodeEvidence {
                stall_reason: reason,
                ..EpisodeEvidence::bare(EPISODE_KIND_STALL_RESTART, "groww", "stall_watchdog", -1)
            };
            assert_eq!(
                classify_episode(&e),
                (BlameClass::Ours, "token_minter_stale"),
                "{reason:?} must classify ours/token_minter_stale"
            );
        }
        // Authorization / Permissions / entitlement class → broker.
        for reason in [
            "authorization violation",
            "permission violation",
            "entitlement",
        ] {
            let e = EpisodeEvidence {
                stall_reason: reason,
                ..EpisodeEvidence::bare(EPISODE_KIND_STALL_RESTART, "groww", "stall_watchdog", -1)
            };
            assert_eq!(
                classify_episode(&e),
                (BlameClass::Broker, "entitlement_reject"),
                "{reason:?} must classify broker/entitlement_reject"
            );
        }
    }

    #[test]
    fn test_classify_resource_pressure_is_ours() {
        // A reset while OUR box is starving (PROC-01/RESOURCE-* ±300s) is ours.
        let mut e = EpisodeEvidence::bare(EPISODE_KIND_DISCONNECT, "dhan", "Dhan or network", -1);
        e.resource_pressure_overlap = true;
        assert_eq!(
            classify_episode(&e),
            (BlameClass::Ours, "resource_pressure")
        );
        // But an authoritative Dhan code still outranks resource pressure.
        let mut with_code =
            EpisodeEvidence::bare(EPISODE_KIND_DISCONNECT, "dhan", "Dhan (another login)", 805);
        with_code.resource_pressure_overlap = true;
        assert_eq!(
            classify_episode(&with_code),
            (BlameClass::Broker, "rate_limit_805")
        );
    }

    #[test]
    fn test_classify_process_death_deploy_vs_crash() {
        // sha changed → a deploy landed → ours/deploy_restart.
        let deploy = EpisodeEvidence {
            build_sha_changed: Some(true),
            ..EpisodeEvidence::bare(EPISODE_KIND_PROCESS_DEATH, "dhan", "boot_reconciled", -1)
        };
        assert_eq!(
            classify_episode(&deploy),
            (BlameClass::Ours, "deploy_restart")
        );
        // sha unchanged → crash / watchdog kill → ours/process_restart.
        let crash = EpisodeEvidence {
            build_sha_changed: Some(false),
            ..EpisodeEvidence::bare(EPISODE_KIND_PROCESS_DEATH, "groww", "boot_reconciled", -1)
        };
        assert_eq!(
            classify_episode(&crash),
            (BlameClass::Ours, "process_restart")
        );
        // Unknown sha (SSM unreachable / `unknown` build) → fail-soft to
        // process_restart — STILL definitionally ours per the directive.
        let unknown =
            EpisodeEvidence::bare(EPISODE_KIND_PROCESS_DEATH, "dhan", "boot_reconciled", -1);
        assert_eq!(
            classify_episode(&unknown),
            (BlameClass::Ours, "process_restart")
        );
    }

    #[test]
    fn test_classify_off_hours_disconnect_is_indeterminate() {
        // Dhan pre/post-market idle cleanup — expected noise, excluded from
        // the headline market-hours count (contract §4).
        let e = EpisodeEvidence::bare(
            EPISODE_KIND_OFF_HOURS_DISCONNECT,
            "dhan",
            "Dhan or network",
            -1,
        );
        assert_eq!(
            classify_episode(&e),
            (BlameClass::Indeterminate, "off_hours_idle")
        );
    }

    #[test]
    fn test_classify_episode_total_unknown_inputs_map_to_indeterminate() {
        // The fail-closed floor: novel kinds / sources / codes never panic,
        // never blank — always indeterminate/unclassified.
        for (kind, source, code) in [
            ("disconnect", "some future source label", -1_i64),
            ("a_new_episode_kind", "n/a", -1),
            ("disconnect", "", 0),
            ("", "", i64::MIN),
            ("disconnect", "groww_resumed", 999_999),
        ] {
            let e = EpisodeEvidence::bare(kind, "dhan", source, code);
            let (class, reason) = classify_episode(&e);
            assert_eq!(
                class,
                BlameClass::Indeterminate,
                "({kind:?},{source:?},{code}) must fail closed to Indeterminate"
            );
            assert_eq!(reason, BLAME_REASON_UNCLASSIFIED);
        }
    }

    proptest! {
        /// No-blank-blame ratchet, layer 3: over ARBITRARY inputs the
        /// classifier always yields a class + a non-empty reason slug and
        /// never panics (totality).
        #[test]
        fn prop_classifier_is_total_never_panics_never_blank(
            kind in ".{0,40}",
            feed in ".{0,16}",
            source in ".{0,64}",
            stall_reason in ".{0,64}",
            dhan_code in proptest::num::i64::ANY,
            b1 in proptest::bool::ANY,
            b2 in proptest::bool::ANY,
            b3 in proptest::bool::ANY,
            b4 in proptest::bool::ANY,
            sha in proptest::option::of(proptest::bool::ANY),
        ) {
            let e = EpisodeEvidence {
                episode_kind: &kind,
                feed: &feed,
                source: &source,
                dhan_code,
                ws_gap9_bare_reset_overlap: b1,
                ws_gap9_429_overlap: b2,
                resilience_peer_evidence: b3,
                resource_pressure_overlap: b4,
                stall_reason: &stall_reason,
                build_sha_changed: sha,
            };
            let (class, reason) = classify_episode(&e);
            prop_assert!(!reason.is_empty(), "reason slug must never be empty");
            prop_assert!(!class.as_str().is_empty());
            // Determinism: same input → same output.
            prop_assert_eq!(classify_episode(&e), (class, reason));
        }
    }
}
