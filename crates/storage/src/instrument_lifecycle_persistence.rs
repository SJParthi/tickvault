//! `instrument_lifecycle` + `instrument_lifecycle_audit` table contracts —
//! Sub-PR of the 2026-05-27 daily-universe expansion (§5 / §6 / §23–§25 / §27).
//!
//! **Status:** CONTRACT ONLY. This module ships the stable identifier
//! surface both tables need:
//!
//! * [`QUESTDB_TABLE_INSTRUMENT_LIFECYCLE`] / [`QUESTDB_TABLE_INSTRUMENT_LIFECYCLE_AUDIT`]
//! * [`DEDUP_KEY_INSTRUMENT_LIFECYCLE`] / [`DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT`]
//! * [`LifecycleState`] — the `lifecycle_state` SYMBOL labels (§5)
//! * [`TransitionKind`] — the audit `transition_kind` SYMBOL labels (§6 + §23)
//! * [`lifecycle_designated_ts_nanos`] — the pinned constant designated
//!   timestamp (see "Designated timestamp" below)
//!
//! The DDL strings, `ensure_*_table` helpers, `*Row` structs, and
//! `append_*_row` helpers land in follow-up sub-PRs (mirroring how
//! `instrument_fetch_audit` shipped its contract in #835 and its
//! persistence helpers in #836). The daily lifecycle reconciler
//! (idempotent UPSERT + state-flip + §24 write-audit-first ordering)
//! is a further follow-up.
//!
//! **Feature-gated** under `daily_universe_fetcher` per rule §21 —
//! dormant in the default build until the boot orchestrator (Sub-PR #10)
//! flips the flag.
//!
//! # Why two tables
//!
//! * `instrument_lifecycle` — the **current** state, ONE row per
//!   instrument EVER observed, UPSERT-updated in place. NEVER deleted
//!   (operator quote §0: "instead of deleting it … marked as expired").
//! * `instrument_lifecycle_audit` — the **forensic chain**, one row per
//!   state transition (appeared / updated / expired / reactivated /
//!   split / segment-moved / …). 5-year SEBI retention. Powers the
//!   §25 point-in-time "what was the universe on date X?" reconstruction.
//!
//! # Designated timestamp (the I-P1-08 resolution)
//!
//! QuestDB requires the designated timestamp column to appear in every
//! `DEDUP UPSERT KEYS(...)` clause (omitting it returns HTTP 400 at
//! boot — the 2026-04-28 regression class). But `instrument_lifecycle`
//! wants ONE row per `(security_id, exchange_segment)` that is UPDATED
//! in place as the instrument's state changes — a *mutable* last-update
//! time in the DEDUP key would make every update a NEW row.
//!
//! Resolution (same as the retired `instrument_persistence` I-P1-08
//! design): the designated `ts` column is pinned to a CONSTANT
//! ([`lifecycle_designated_ts_nanos`] = epoch 0) so the DEDUP fires on
//! the business key `(security_id, exchange_segment)` alone, while the
//! mutable wall-clock last-update time lives in a separate
//! `last_update_ts` column (added by the DDL follow-up). The DEDUP key
//! constant therefore lists `ts` first to satisfy QuestDB, then the
//! I-P1-11 composite business key.
//!
//! The `*_audit` table is append-only (not UPSERT-in-place), so its `ts`
//! is the real per-transition wall-clock; its DEDUP key additionally
//! carries `transition_kind` so two different transitions for the same
//! instrument on the same day both survive (§6).
//!
//! ## 2026-05-29 — bulk writes use ILP (port 9009), NOT `/exec` URL
//!
//! Both tables are `timestamp(ts) PARTITION BY DAY WAL DEDUP UPSERT KEYS(...)`
//! (see the DDLs below — the "PARTITION BY NONE" prose in the historical note
//! that follows is SUPERSEDED). With the lifecycle table's constant designated
//! `ts`, every never-deleted row lands in one partition — harmless, and WAL +
//! DEDUP work correctly. The BULK writers
//! ([`append_instrument_lifecycle_rows`] / [`append_instrument_lifecycle_audit_rows`])
//! ingest via **ILP** (the same pipe ticks/candles use) — NOT multi-row SQL
//! `INSERT` over `/exec`. Reason: `/exec` carries the SQL in the URL query
//! string, capped by QuestDB's request buffer; at the ~79K-row applicable-F&O
//! master this overflowed (the 2026-05-29 "79190 write error(s)" /
//! "connection closed before message completed" boot halts). ILP has no URL
//! limit. The single-row helpers + DDL stay on `/exec` (small, no URL issue).
//!
//! **⚠ CORRECTED 2026-10-11 (audit QDB-2) — the bulk writers now use ILP over
//! HTTP, not TCP port 9009.** TCP ILP gets no reply from the server: a line
//! QuestDB refused closed the socket while `flush()` still returned `Ok`, so a
//! SEBI never-delete row could be logged as written and never stored. HTTP ILP
//! waits for the answer on every flush (see `sebi_master_ilp_http_conf`). The
//! "ILP, not `/exec`" reasoning above is unchanged.
//!
//! ## ⚠ DDL follow-up — historical note (SUPERSEDED above)
//!
//! Because `instrument_lifecycle`'s designated `ts` is pinned to epoch 0,
//! the table's DDL **MUST use `PARTITION BY NONE`** (or partition on a
//! separate non-designated date column) — NOT `PARTITION BY DAY/MONTH`.
//! A date-partitioned table with a constant `ts` would funnel every
//! never-deleted lifecycle row into a single `1970-01-01` partition,
//! defeating QuestDB partition pruning AND breaking the `partition_manager`
//! S3-archive-by-date lifecycle (it detaches partitions by date; a
//! one-partition table can never detach). The append-only
//! `instrument_lifecycle_audit` table keeps a REAL per-transition `ts`,
//! so it partitions by day normally.
//!
//! # Cross-references
//!
//! * Rule §5 / §6 / §23 / §24 / §25 / §27 — `daily-universe-scope-expansion-2026-05-27.md`
//! * `security-id-uniqueness.md` (I-P1-11) — composite `(security_id, exchange_segment)`
//! * `gap-enforcement.md` (I-P1-08) — constant designated timestamp for UPSERT-one-row tables
//!
//! # ⚠ CHANGED 2026-10-28 (owner Quote 44) — no writer, both tables frozen
//!
//! The owner stopped the daily write of both tables: nothing on the live path
//! read them (subscriptions, contracts, candles, ticks and orders come from
//! the morning artifact files), and the lifecycle table had been appending a
//! full copy of the master every day (18,784,013 rows for 161,886 ids,
//! measured 2026-10-28). The DDL, `ensure_*`, row structs, ILP builders,
//! append helpers and state-update SQL were deleted with their only caller.
//! What stays is the identifier surface the keep-forever, retention-exempt,
//! rebuild-keep and wipe-keep lists and the pure reconciler name: the table
//! names, the DEDUP keys, the state/transition labels, and the HTTP ILP config
//! the `index_constituency` writer still uses. Both tables and every row in
//! them are KEPT, frozen as history (§25: never deleted). Point-in-time
//! questions after 2026-10-27 are answered from the dated instrument
//! snapshot files copied to `s3://tv-<env>-cold/instrument-snapshots/<date>/`
//! (`crates/app/src/instrument_snapshot_upload.rs`). A daily write to either
//! table, or a replacement table, needs a fresh dated quote first.

use tickvault_common::config::QuestDbConfig;

/// Per-request timeout (ms) for the SEBI never-delete ILP writer
/// (`index_constituency`; the two lifecycle writers were deleted 2026-10-28,
/// Quote 44). Generous because one request can carry a whole day of rows;
/// the client adds its own per-byte allowance on top
/// (`request_min_throughput`).
pub(crate) const SEBI_MASTER_ILP_REQUEST_TIMEOUT_MS: u64 = 60_000;

/// ILP config string for the SEBI never-delete writers — HTTP, never TCP.
///
/// **2026-10-11 (audit QDB-2).** These writers used the TCP ILP pipe. TCP ILP
/// gets no reply: a line QuestDB rejects makes the server drop the socket
/// while `flush()` still returns `Ok`, so the caller logged success for SEBI
/// rows that were never stored. HTTP ILP waits for the server's answer on
/// every flush, so a refused chunk now returns `Err` and the caller's
/// existing failure arm fires.
///
/// **⚠ CORRECTED 2026-10-11 (review of QDB-2, same day).** This shipped with
/// `retry_timeout=0` and the reason "the caller already treats a failed chunk
/// as 'next idempotent run re-writes it'". That reason is false for two of
/// the three tables: `instrument_lifecycle_audit` rows are derived from the
/// difference against the previous state, which a landed lifecycle row
/// erases, and `index_constituency` is a per-date point-in-time row that
/// tomorrow's run does not write. So a brief QuestDB hiccup became a
/// permanent hole. These writers run once a day on `spawn_blocking`, never on
/// the frame drain, so they now retry for
/// [`SEBI_MASTER_ILP_RETRY_TIMEOUT_MS`]. The client retries only transport
/// failures and 5xx answers; a 4xx refusal (a bad row) is never retried.
///
/// **2026-10-28 (Quote 44):** the two lifecycle writers that shared this are
/// deleted; `index_constituency_persistence` is its only caller.
pub(crate) fn sebi_master_ilp_http_conf(cfg: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout={};request_timeout={};",
        cfg.host,
        cfg.http_port,
        SEBI_MASTER_ILP_RETRY_TIMEOUT_MS,
        SEBI_MASTER_ILP_REQUEST_TIMEOUT_MS
    )
}

/// How long (ms) the SEBI never-delete ILP writers retry a transport failure
/// or a 5xx answer before giving up on a chunk. Cold path (once a day, on
/// `spawn_blocking`), so a few seconds of retry costs nothing that waits.
pub(crate) const SEBI_MASTER_ILP_RETRY_TIMEOUT_MS: u64 = 10_000;

/// Wire-format table name for the current-state lifecycle table.
/// Stable across releases — operators, the reconciler, and the
/// `partition_manager` S3 archive job depend on the exact string.
pub const QUESTDB_TABLE_INSTRUMENT_LIFECYCLE: &str = "instrument_lifecycle";

/// Wire-format table name for the forensic transition-chain table.
pub const QUESTDB_TABLE_INSTRUMENT_LIFECYCLE_AUDIT: &str = "instrument_lifecycle_audit";

/// DEDUP key for `instrument_lifecycle` — UPSERT one row per instrument.
///
/// `ts` (pinned constant — see module docs) satisfies QuestDB's
/// designated-timestamp-in-DEDUP requirement; `security_id` +
/// `exchange_segment` are the I-P1-11 composite business key (Dhan
/// reuses `security_id` across segments, so segment is mandatory).
pub const DEDUP_KEY_INSTRUMENT_LIFECYCLE: &str = "ts, security_id, exchange_segment, feed";

/// DEDUP key for `instrument_lifecycle_audit` — append one row per
/// transition.
///
/// `ts` is the real per-transition wall-clock designated timestamp.
/// `trading_date_ist` is the partition/business date. `security_id` +
/// `exchange_segment` is the I-P1-11 composite key. `transition_kind`
/// ensures two distinct transitions for the same instrument on the same
/// day (e.g. `updated` then `expired`) BOTH survive rather than the
/// terminal one overwriting the trigger one (§6).
// Kept on one line so the `dedup_segment_meta_guard.rs` source scanner
// (which matches `const … : &str = "…"` on a single line) detects it and
// verifies both the I-P1-11 segment-pairing and the designated-`ts` token.
#[rustfmt::skip]
pub const DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT: &str = "ts, trading_date_ist, security_id, exchange_segment, transition_kind, feed";

/// The pinned constant designated timestamp (epoch 0) for the
/// UPSERT-in-place `instrument_lifecycle` table. See module docs
/// ("Designated timestamp") for the I-P1-08 rationale. The DDL
/// follow-up stamps every lifecycle row's designated `ts` with this
/// value so the DEDUP fires on the business key alone.
#[must_use]
pub const fn lifecycle_designated_ts_nanos() -> i64 {
    0
}

/// Stable wire-format strings for the `instrument_lifecycle.lifecycle_state`
/// SYMBOL column (§5). Operators query this column by exact string match;
/// bumping any label is a breaking change.
///
/// The reconciler (follow-up) flips `Active` → one of the `Expired*`
/// states when an instrument disappears from the daily CSV, and back to
/// `Active` on reappearance. `Delisted` is operator-set (manual). Rows
/// are NEVER deleted (§0 operator lock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LifecycleState {
    /// Present in today's CSV and tradable.
    Active,
    /// Stock dropped out of the F&O list (its derivatives expired off it).
    ExpiredFromFno,
    /// A specific derivative contract reached its expiry date.
    ExpiredContract,
    /// An index that is no longer published.
    ExpiredIndex,
    /// Operator-set terminal state (manual override) — instrument removed
    /// from the exchange entirely.
    Delisted,
}

impl LifecycleState {
    /// Returns the wire-format SYMBOL label. ALWAYS lowercase snake_case.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::ExpiredFromFno => "expired_from_fno",
            Self::ExpiredContract => "expired_contract",
            Self::ExpiredIndex => "expired_index",
            Self::Delisted => "delisted",
        }
    }

    /// Parse a wire-format SYMBOL label back into a [`LifecycleState`].
    /// Returns `None` for any unrecognized string — the read-back loader
    /// counts these as "unknown state" rather than guessing, so a future
    /// schema drift surfaces instead of silently mis-classifying.
    /// Exact inverse of [`as_str`](Self::as_str).
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "expired_from_fno" => Some(Self::ExpiredFromFno),
            "expired_contract" => Some(Self::ExpiredContract),
            "expired_index" => Some(Self::ExpiredIndex),
            "delisted" => Some(Self::Delisted),
            _ => None,
        }
    }

    /// True only for `Active`. Dashboards + the subscription dispatcher
    /// MUST filter `WHERE lifecycle_state = 'active'`.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    /// True for any of the three `Expired*` states (NOT `Delisted`,
    /// which is a manual terminal state, and NOT `Active`).
    #[must_use]
    pub const fn is_expired(self) -> bool {
        matches!(
            self,
            Self::ExpiredFromFno | Self::ExpiredContract | Self::ExpiredIndex
        )
    }

    /// All variants, for exhaustive ratchet tests.
    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::Active,
            Self::ExpiredFromFno,
            Self::ExpiredContract,
            Self::ExpiredIndex,
            Self::Delisted,
        ]
    }
}

/// Stable wire-format strings for the
/// `instrument_lifecycle_audit.transition_kind` SYMBOL column (§6 + §23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransitionKind {
    /// First time this `(security_id, exchange_segment)` was ever seen.
    Appeared,
    /// A mutable field (lot_size, tick_size, symbol, …) changed.
    Updated,
    /// `lifecycle_state` flipped to one of the `Expired*` states.
    Expired,
    /// An expired instrument reappeared in the CSV and flipped back to
    /// `Active`.
    Reactivated,
    /// Operator manually set `Delisted`.
    DelistedManual,
    /// Operator set `lifecycle_state_locked` (§5 override).
    Locked,
    /// Stock split detected — lot/tick size halved or less (§23).
    Split,
    /// Same `security_id` moved to a different `exchange_segment` (§23).
    SegmentMoved,
}

impl TransitionKind {
    /// Returns the wire-format SYMBOL label. ALWAYS lowercase snake_case.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Appeared => "appeared",
            Self::Updated => "updated",
            Self::Expired => "expired",
            Self::Reactivated => "reactivated",
            Self::DelistedManual => "delisted_manual",
            Self::Locked => "locked",
            Self::Split => "split",
            Self::SegmentMoved => "segment_moved",
        }
    }

    /// All variants, for exhaustive ratchet tests.
    #[must_use]
    pub const fn all() -> [Self; 8] {
        [
            Self::Appeared,
            Self::Updated,
            Self::Expired,
            Self::Reactivated,
            Self::DelistedManual,
            Self::Locked,
            Self::Split,
            Self::SegmentMoved,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_table_name_constants() {
        assert_eq!(QUESTDB_TABLE_INSTRUMENT_LIFECYCLE, "instrument_lifecycle");
        assert_eq!(
            QUESTDB_TABLE_INSTRUMENT_LIFECYCLE_AUDIT,
            "instrument_lifecycle_audit"
        );
    }

    #[test]
    fn test_lifecycle_dedup_key_includes_ts_and_segment() {
        // QuestDB designated-timestamp requirement + I-P1-11 segment.
        let has_ts = DEDUP_KEY_INSTRUMENT_LIFECYCLE
            .split([',', ' '])
            .map(str::trim)
            .any(|t| t == "ts");
        assert!(
            has_ts,
            "lifecycle DEDUP key must include the bare `ts` token"
        );
        assert!(DEDUP_KEY_INSTRUMENT_LIFECYCLE.contains("security_id"));
        assert!(
            DEDUP_KEY_INSTRUMENT_LIFECYCLE.contains("exchange_segment"),
            "I-P1-11: security_id must be paired with exchange_segment"
        );
        assert!(
            DEDUP_KEY_INSTRUMENT_LIFECYCLE
                .split([',', ' '])
                .map(str::trim)
                .any(|t| t == "feed"),
            "operator override 2026-06-28: feed must be in the DEDUP key"
        );
        assert_eq!(
            DEDUP_KEY_INSTRUMENT_LIFECYCLE.matches(',').count() + 1,
            4,
            "lifecycle DEDUP key has exactly 4 columns (ts, security_id, exchange_segment, feed)"
        );
    }

    #[test]
    fn test_lifecycle_audit_dedup_key_includes_ts_segment_and_transition_kind() {
        let has_ts = DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT
            .split([',', ' '])
            .map(str::trim)
            .any(|t| t == "ts");
        assert!(has_ts, "audit DEDUP key must include the bare `ts` token");
        assert!(DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT.contains("trading_date_ist"));
        assert!(DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT.contains("exchange_segment"));
        assert!(
            DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT.contains("transition_kind"),
            "without transition_kind two same-day transitions for one \
             instrument would collapse to a single row (§6)"
        );
        assert!(
            DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT
                .split([',', ' '])
                .map(str::trim)
                .any(|t| t == "feed"),
            "operator override 2026-06-28: feed must be in the audit DEDUP key"
        );
        assert_eq!(
            DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT.matches(',').count() + 1,
            6,
            "audit DEDUP key has exactly 6 columns (… + feed)"
        );
    }

    #[test]
    fn test_lifecycle_designated_ts_is_constant_zero() {
        // I-P1-08: pinned constant so business-key DEDUP works for the
        // UPSERT-in-place lifecycle table.
        assert_eq!(lifecycle_designated_ts_nanos(), 0);
        assert_eq!(
            lifecycle_designated_ts_nanos(),
            lifecycle_designated_ts_nanos(),
            "must be constant across calls"
        );
    }

    #[test]
    fn test_lifecycle_state_has_five_distinct_lowercase_labels() {
        let variants = LifecycleState::all();
        assert_eq!(variants.len(), 5);
        let mut seen = Vec::new();
        for v in variants {
            let label = v.as_str();
            assert!(!seen.contains(&label), "duplicate label {label}");
            for ch in label.chars() {
                assert!(
                    ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_',
                    "non-snake_case char `{ch}` in `{label}`"
                );
            }
            seen.push(label);
        }
    }

    #[test]
    fn test_lifecycle_state_is_active_only_for_active() {
        assert!(LifecycleState::Active.is_active());
        for v in LifecycleState::all() {
            if v != LifecycleState::Active {
                assert!(!v.is_active(), "{v:?} must not be active");
            }
        }
        let active_count = LifecycleState::all()
            .iter()
            .filter(|v| v.is_active())
            .count();
        assert_eq!(active_count, 1);
    }

    #[test]
    fn test_lifecycle_state_is_expired_covers_three_expired_states_only() {
        assert!(LifecycleState::ExpiredFromFno.is_expired());
        assert!(LifecycleState::ExpiredContract.is_expired());
        assert!(LifecycleState::ExpiredIndex.is_expired());
        assert!(!LifecycleState::Active.is_expired());
        assert!(
            !LifecycleState::Delisted.is_expired(),
            "Delisted is a manual terminal state, distinct from Expired*"
        );
        let expired_count = LifecycleState::all()
            .iter()
            .filter(|v| v.is_expired())
            .count();
        assert_eq!(expired_count, 3);
    }

    #[test]
    fn test_lifecycle_state_active_and_expired_are_disjoint() {
        for v in LifecycleState::all() {
            assert!(
                !(v.is_active() && v.is_expired()),
                "{v:?} cannot be both active and expired"
            );
        }
    }

    #[test]
    fn test_transition_kind_has_eight_distinct_lowercase_labels() {
        let variants = TransitionKind::all();
        assert_eq!(variants.len(), 8);
        let mut seen = Vec::new();
        for v in variants {
            let label = v.as_str();
            assert!(!seen.contains(&label), "duplicate label {label}");
            assert!(!label.is_empty());
            for ch in label.chars() {
                assert!(
                    ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_',
                    "non-snake_case char `{ch}` in `{label}`"
                );
            }
            seen.push(label);
        }
    }

    #[test]
    fn test_transition_kind_wire_labels_are_stable() {
        // Pin the exact strings — operators + the §25 point-in-time
        // query depend on them.
        assert_eq!(TransitionKind::Appeared.as_str(), "appeared");
        assert_eq!(TransitionKind::Updated.as_str(), "updated");
        assert_eq!(TransitionKind::Expired.as_str(), "expired");
        assert_eq!(TransitionKind::Reactivated.as_str(), "reactivated");
        assert_eq!(TransitionKind::DelistedManual.as_str(), "delisted_manual");
        assert_eq!(TransitionKind::Locked.as_str(), "locked");
        assert_eq!(TransitionKind::Split.as_str(), "split");
        assert_eq!(TransitionKind::SegmentMoved.as_str(), "segment_moved");
    }

    #[test]
    fn test_lifecycle_state_wire_labels_are_stable() {
        assert_eq!(LifecycleState::Active.as_str(), "active");
        assert_eq!(LifecycleState::ExpiredFromFno.as_str(), "expired_from_fno");
        assert_eq!(LifecycleState::ExpiredContract.as_str(), "expired_contract");
        assert_eq!(LifecycleState::ExpiredIndex.as_str(), "expired_index");
        assert_eq!(LifecycleState::Delisted.as_str(), "delisted");
    }

    #[test]
    fn test_lifecycle_state_from_wire_roundtrips_every_variant() {
        for v in LifecycleState::all() {
            assert_eq!(
                LifecycleState::from_wire(v.as_str()),
                Some(v),
                "from_wire(as_str()) must roundtrip for {v:?}"
            );
        }
    }

    #[test]
    fn test_lifecycle_state_from_wire_rejects_unknown() {
        assert_eq!(LifecycleState::from_wire("nonsense"), None);
        assert_eq!(LifecycleState::from_wire(""), None);
        assert_eq!(LifecycleState::from_wire("ACTIVE"), None, "case-sensitive");
    }

    #[test]
    fn test_enums_derive_eq_and_hash() {
        use std::collections::HashSet;
        let mut s: HashSet<LifecycleState> = HashSet::new();
        for v in LifecycleState::all() {
            assert!(s.insert(v));
        }
        let mut t: HashSet<TransitionKind> = HashSet::new();
        for v in TransitionKind::all() {
            assert!(t.insert(v));
        }
    }

    fn cfg_with_ports(http_port: u16, ilp_port: u16) -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port,
            pg_port: 1,
            ilp_port,
        }
    }

    #[test]
    fn test_sebi_master_ilp_conf_is_http_with_request_timeout() {
        let conf = sebi_master_ilp_http_conf(&cfg_with_ports(9000, 9009));
        assert!(conf.starts_with("http::addr=127.0.0.1:9000;"), "{conf}");
        assert!(conf.contains("protocol_version=1;"), "{conf}");
        assert!(conf.contains("retry_timeout=10000;"), "{conf}");
        assert_eq!(SEBI_MASTER_ILP_RETRY_TIMEOUT_MS, 10_000);
        assert!(conf.contains("request_timeout=60000;"), "{conf}");
        assert_eq!(SEBI_MASTER_ILP_REQUEST_TIMEOUT_MS, 60_000);
        assert!(!conf.contains("tcp::addr"), "{conf}");
        assert!(
            !conf.contains("9009"),
            "the ILP TCP port must not be used: {conf}"
        );
    }

    /// Quote 44 (2026-10-28): both tables are frozen history. The non-test
    /// half must carry no DDL, no insert, no ILP sender and no UPDATE, so a
    /// writer cannot come back by accident (a fresh dated quote is required).
    #[test]
    fn test_instrument_lifecycle_module_has_no_writer() {
        let source = include_str!("instrument_lifecycle_persistence.rs");
        let prod = source
            .split(concat!("#[cfg(", "test)]"))
            .next()
            .expect("source has a non-test half");
        for banned in [
            concat!("CREATE ", "TABLE"),
            concat!("INSERT ", "INTO"),
            concat!("UPDATE ", "{"),
            concat!("Sender::", "from_conf"),
            concat!("fn append_", "instrument_lifecycle"),
            concat!("fn ensure_", "instrument_lifecycle"),
            concat!("fn update_", "lifecycle_state"),
            concat!("tcp", "::addr"),
        ] {
            assert!(
                !prod.contains(banned),
                "instrument_lifecycle_persistence must not write (Quote 44): found `{banned}`"
            );
        }
    }
}
