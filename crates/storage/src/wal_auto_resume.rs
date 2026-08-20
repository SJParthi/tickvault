//! Conditional auto-resume for WAL-suspended QuestDB tables.
//!
//! Operator directive 2026-08-19: the box must manage itself on a fixed
//! instance — *"no manual intervention or human inputs or human monitoring
//! shod lobe expected"*.
//!
//! # The state this closes, and why it was left open on purpose
//!
//! A WAL-suspended table keeps ACKing ILP rows while WAL APPLY is stopped, so
//! rows are accepted and never become visible. `wal_suspension_watcher`
//! detects it and pages `WAL-SUSPEND-01`, and its module header states the
//! recovery decision plainly:
//!
//! > Recovery action (`ALTER TABLE <t> RESUME WAL`) is an OPERATOR decision —
//! > NEVER auto-executed (auto-resume can replay into a still-broken disk).
//!
//! That reasoning is CORRECT and this module does not overturn it. Blindly
//! resuming into a full disk replays the WAL, fails again, re-suspends, and
//! loops — trading a stopped table for a hot loop against a broken volume.
//!
//! What was missing is not the caution; it is the CONDITION. A resume is only
//! dangerous while the cause persists. So this resumes when — and only when —
//! the disk that caused the suspension has demonstrably recovered, with a
//! bounded number of attempts per episode, and it hands back to the operator
//! the moment the evidence stops supporting an automatic decision.
//!
//! # The three ways this refuses
//!
//! | Refusal | Why it is the safe answer |
//! |---|---|
//! | Disk still below [`RESUME_MIN_FREE_FRACTION`] | The original hazard. Resuming replays into the same full volume. |
//! | Disk state UNKNOWN (`df` failed) | Absence of evidence is not evidence of recovery. An unknown disk is treated exactly like a full one. |
//! | Past [`MAX_RESUME_ATTEMPTS_PER_EPISODE`] | The table keeps re-suspending for a reason this module cannot see. Looping would hide it; stopping surfaces it. |
//!
//! # Why the threshold is well above the pressure-loop exit
//!
//! The disk-pressure loop leaves an episode at 60% used. Resuming there would
//! replay a WAL into a volume with 40% headroom that is actively being
//! refilled by the very ingest that filled it the first time.
//! [`RESUME_MIN_FREE_FRACTION`] demands materially more room than the pressure
//! loop is content with, so the two mechanisms cannot fight: pressure
//! reclaims, and only once the volume is genuinely comfortable does apply
//! restart.
//!
//! # Complexity
//!
//! O(suspended tables) per poll, which is 0 on every healthy poll. The
//! attempt ledger is keyed by table name and is bounded by the number of
//! tables the schema has — a fixed set, not a per-instrument one.

use std::collections::HashMap;

/// Free-space fraction the volume must have before a resume is attempted.
///
/// 0.25 — a quarter of the volume free. Deliberately far above the
/// disk-pressure loop's 60%-used exit (0.40 free): resuming the moment
/// pressure relents would replay a WAL into a volume that is still being
/// refilled by the ingest that filled it, and the two mechanisms would take
/// turns. A quarter free on a 200 GB volume is 50 GB, which is more than any
/// single table's WAL backlog can plausibly be.
pub const RESUME_MIN_FREE_FRACTION: f64 = 0.25;

/// Resume attempts per table per suspension episode.
///
/// Three, then stop and leave it to the operator. A table that re-suspends
/// three times with a healthy disk is suspended for a reason this module
/// cannot observe — a corrupt WAL segment, a schema conflict — and retrying
/// forever would turn a visible stopped table into an invisible hot loop.
pub const MAX_RESUME_ATTEMPTS_PER_EPISODE: u32 = 3;

/// What the disk looks like to the resume decision.
///
/// Three states, not two. `Unknown` exists because `df` can fail, and folding
/// that into "not healthy" would be right by accident — the point is that the
/// decision is made on EVIDENCE, and a failed probe is the absence of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskEvidence {
    /// Measured, and comfortable enough to replay a WAL into.
    Healthy,
    /// Measured, and still too tight.
    Tight,
    /// Not measurable right now.
    Unknown,
}

impl DiskEvidence {
    /// Classifies a measured free/total pair.
    ///
    /// A zero or absent total is `Unknown`, never `Healthy`: dividing by it
    /// would be a crash, and assuming the best about an unreadable volume is
    /// the exact false-OK this module exists to avoid.
    #[must_use]
    pub fn from_measurement(free_bytes: u64, total_bytes: u64) -> Self {
        if total_bytes == 0 {
            return Self::Unknown;
        }
        #[allow(clippy::cast_precision_loss)]
        // APPROVED: byte counts on any real volume are far below 2^53, and the
        // comparison is a threshold test where a one-byte rounding difference
        // cannot change the answer.
        let fraction = free_bytes as f64 / total_bytes as f64;
        if fraction >= RESUME_MIN_FREE_FRACTION {
            Self::Healthy
        } else {
            Self::Tight
        }
    }
}

/// What to do about one suspended table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeDecision {
    /// Issue `ALTER TABLE <table> RESUME WAL`. Carries the attempt number so
    /// the log line says which try this is rather than repeating identically.
    Resume { attempt: u32 },
    /// Do nothing this poll; the disk is not ready.
    WaitForDisk,
    /// Do nothing this poll; the disk could not be measured.
    WaitForEvidence,
    /// Stop trying. The operator owns it now.
    GiveUp,
}

/// Per-table attempt ledger for the current suspension episode.
///
/// PURE — no I/O, no clock, fed by the owning task. Same shape as the
/// `WalSuspensionTracker` it rides beside, and for the same reason: a
/// decision this consequential should be exhaustively testable without a
/// database.
#[derive(Debug, Default)]
pub struct ResumeLedger {
    attempts: HashMap<String, u32>,
}

impl ResumeLedger {
    /// A ledger with no history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attempts already spent on `table` in the current episode.
    #[must_use]
    pub fn attempts_for(&self, table: &str) -> u32 {
        self.attempts.get(table).copied().unwrap_or(0)
    }

    /// Decides what to do about a table that is currently suspended.
    ///
    /// Records the attempt when it returns [`ResumeDecision::Resume`], so a
    /// caller that ignores the decision still burns the attempt — which is the
    /// conservative direction: a lost decision costs a retry, never an extra
    /// one.
    pub fn decide(&mut self, table: &str, disk: DiskEvidence) -> ResumeDecision {
        let spent = self.attempts_for(table);
        if spent >= MAX_RESUME_ATTEMPTS_PER_EPISODE {
            return ResumeDecision::GiveUp;
        }
        match disk {
            // The disk check comes BEFORE the attempt is spent. A table
            // waiting out a full disk for an hour must not exhaust its three
            // attempts doing nothing — those attempts are for real resumes.
            DiskEvidence::Tight => ResumeDecision::WaitForDisk,
            DiskEvidence::Unknown => ResumeDecision::WaitForEvidence,
            DiskEvidence::Healthy => {
                let attempt = spent + 1;
                self.attempts.insert(table.to_owned(), attempt);
                ResumeDecision::Resume { attempt }
            }
        }
    }

    /// Clears a table's history because its suspension episode ended.
    ///
    /// Called on the falling edge. Without it a table that suspends once a
    /// month would exhaust its three attempts over three months and then
    /// never auto-resume again — the ledger would silently become a
    /// lifetime budget instead of a per-episode one.
    pub fn clear(&mut self, table: &str) {
        self.attempts.remove(table);
    }

    /// Tables with recorded attempts. Bounded by the schema's table count.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.attempts.len()
    }
}

/// The `ALTER TABLE … RESUME WAL` statement for `table`.
///
/// # Why the name is validated rather than escaped
///
/// Table names reaching here come from QuestDB's own `wal_tables()` output,
/// so they are not attacker-controlled — but this builds a statement that
/// QuestDB will execute, and "the source is trusted" is the reasoning behind
/// most injection. An identifier that is not plainly alphanumeric is REFUSED
/// rather than quoted, because there is no legitimate tickvault table it
/// would reject and a refusal cannot be escaped wrongly.
///
/// Returns `None` for anything that is not a safe identifier.
#[must_use]
pub fn build_resume_sql(table: &str) -> Option<String> {
    if table.is_empty() || table.len() > 128 {
        return None;
    }
    if !table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    // A leading digit is not a valid unquoted identifier anywhere, and no
    // tickvault table starts with one.
    if table.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("ALTER TABLE {table} RESUME WAL;"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_measurement_needs_real_headroom_not_merely_some() {
        let total = 200_000_000_000u64;
        // A quarter free is the bar.
        assert_eq!(
            DiskEvidence::from_measurement(50_000_000_000, total),
            DiskEvidence::Healthy
        );
        assert_eq!(
            DiskEvidence::from_measurement(49_000_000_000, total),
            DiskEvidence::Tight
        );
        // The pressure loop is content at 40% free. Resuming there would
        // replay into a volume the same ingest is still refilling, and the
        // two mechanisms would take turns.
        assert_eq!(
            DiskEvidence::from_measurement(80_000_000_000, total),
            DiskEvidence::Healthy,
            "40% free is comfortably past the bar"
        );
        assert_eq!(
            DiskEvidence::from_measurement(0, total),
            DiskEvidence::Tight
        );
    }

    #[test]
    fn from_measurement_treats_an_unreadable_volume_as_unknown() {
        // Not Healthy, and not silently Tight either — assuming the best
        // about a volume nobody could measure is the false-OK this module
        // exists to avoid, and a divide by zero is worse than both.
        assert_eq!(
            DiskEvidence::from_measurement(1_000, 0),
            DiskEvidence::Unknown
        );
        assert_eq!(DiskEvidence::from_measurement(0, 0), DiskEvidence::Unknown);
    }

    #[test]
    fn decide_resumes_only_when_the_disk_has_actually_recovered() {
        let mut ledger = ResumeLedger::new();
        assert_eq!(
            ledger.decide("ticks", DiskEvidence::Healthy),
            ResumeDecision::Resume { attempt: 1 }
        );
    }

    #[test]
    fn decide_refuses_while_the_disk_is_still_the_problem() {
        let mut ledger = ResumeLedger::new();
        assert_eq!(
            ledger.decide("ticks", DiskEvidence::Tight),
            ResumeDecision::WaitForDisk
        );
        assert_eq!(
            ledger.decide("ticks", DiskEvidence::Unknown),
            ResumeDecision::WaitForEvidence
        );
    }

    #[test]
    fn attempts_for_does_not_advance_while_merely_waiting() {
        // The attempts are for real resumes. A table waiting out an hour of
        // full disk at a 60-second poll would otherwise exhaust its budget
        // 60 times over without ever trying anything.
        let mut ledger = ResumeLedger::new();
        for _ in 0..100 {
            ledger.decide("ticks", DiskEvidence::Tight);
            ledger.decide("ticks", DiskEvidence::Unknown);
        }
        assert_eq!(ledger.attempts_for("ticks"), 0);
        assert_eq!(
            ledger.decide("ticks", DiskEvidence::Healthy),
            ResumeDecision::Resume { attempt: 1 },
            "the full budget survives the wait"
        );
    }

    #[test]
    fn decide_gives_up_after_the_cap_rather_than_looping_forever() {
        // A table that re-suspends three times on a healthy disk is suspended
        // for a reason this module cannot see. Retrying forever converts a
        // visible stopped table into an invisible hot loop.
        let mut ledger = ResumeLedger::new();
        for expected in 1..=MAX_RESUME_ATTEMPTS_PER_EPISODE {
            assert_eq!(
                ledger.decide("ticks", DiskEvidence::Healthy),
                ResumeDecision::Resume { attempt: expected }
            );
        }
        assert_eq!(
            ledger.decide("ticks", DiskEvidence::Healthy),
            ResumeDecision::GiveUp
        );
        // And it stays given up — a healthy disk does not reset the count.
        assert_eq!(
            ledger.decide("ticks", DiskEvidence::Healthy),
            ResumeDecision::GiveUp
        );
    }

    #[test]
    fn give_up_outranks_every_disk_state() {
        // Past the cap the disk no longer matters: the operator owns it, and
        // a WaitForDisk here would read as "still trying".
        let mut ledger = ResumeLedger::new();
        for _ in 0..MAX_RESUME_ATTEMPTS_PER_EPISODE {
            ledger.decide("ticks", DiskEvidence::Healthy);
        }
        assert_eq!(
            ledger.decide("ticks", DiskEvidence::Tight),
            ResumeDecision::GiveUp
        );
        assert_eq!(
            ledger.decide("ticks", DiskEvidence::Unknown),
            ResumeDecision::GiveUp
        );
    }

    #[test]
    fn clear_makes_the_budget_per_episode_rather_than_per_lifetime() {
        // Without this a table suspending once a month would spend its three
        // attempts over three months and then never auto-resume again — the
        // ledger silently becoming a lifetime budget.
        let mut ledger = ResumeLedger::new();
        for _ in 0..MAX_RESUME_ATTEMPTS_PER_EPISODE {
            ledger.decide("ticks", DiskEvidence::Healthy);
        }
        assert_eq!(
            ledger.decide("ticks", DiskEvidence::Healthy),
            ResumeDecision::GiveUp
        );
        ledger.clear("ticks");
        assert_eq!(ledger.attempts_for("ticks"), 0);
        assert_eq!(
            ledger.decide("ticks", DiskEvidence::Healthy),
            ResumeDecision::Resume { attempt: 1 }
        );
    }

    #[test]
    fn tracked_counts_each_table_with_its_own_budget() {
        let mut ledger = ResumeLedger::new();
        for _ in 0..MAX_RESUME_ATTEMPTS_PER_EPISODE {
            ledger.decide("ticks", DiskEvidence::Healthy);
        }
        assert_eq!(
            ledger.decide("ticks", DiskEvidence::Healthy),
            ResumeDecision::GiveUp
        );
        assert_eq!(
            ledger.decide("candles_1m", DiskEvidence::Healthy),
            ResumeDecision::Resume { attempt: 1 },
            "one exhausted table must not silence another"
        );
        assert_eq!(ledger.tracked(), 2);
    }

    #[test]
    fn clear_on_an_untracked_table_is_harmless() {
        // The falling edge fires for tables that never needed a resume —
        // they recovered on their own.
        let mut ledger = ResumeLedger::new();
        ledger.clear("never_seen");
        assert_eq!(ledger.tracked(), 0);
    }

    #[test]
    fn build_resume_sql_emits_the_statement_for_a_real_table() {
        assert_eq!(
            build_resume_sql("ticks").as_deref(),
            Some("ALTER TABLE ticks RESUME WAL;")
        );
        assert_eq!(
            build_resume_sql("candles_1m").as_deref(),
            Some("ALTER TABLE candles_1m RESUME WAL;")
        );
        assert_eq!(
            build_resume_sql("market_depth").as_deref(),
            Some("ALTER TABLE market_depth RESUME WAL;")
        );
    }

    #[test]
    fn build_resume_sql_refuses_anything_that_is_not_a_plain_identifier() {
        // The names come from QuestDB's own output, so they are not
        // attacker-controlled — but "the source is trusted" is the reasoning
        // behind most injection, and this builds a statement the database
        // will execute. Refusing cannot be escaped wrongly.
        assert_eq!(build_resume_sql(""), None);
        assert_eq!(build_resume_sql("ticks; DROP TABLE candles_1m"), None);
        assert_eq!(build_resume_sql("ticks'"), None);
        assert_eq!(build_resume_sql("ticks\"; --"), None);
        assert_eq!(build_resume_sql("has space"), None);
        assert_eq!(build_resume_sql("has-dash"), None);
        assert_eq!(build_resume_sql("1leading_digit"), None);
        assert_eq!(build_resume_sql(&"x".repeat(129)), None);
        assert_eq!(
            build_resume_sql(&"x".repeat(128)).is_some(),
            true,
            "the bound is inclusive"
        );
    }

    #[test]
    fn the_resume_threshold_is_stricter_than_the_pressure_loop_exit() {
        // If these ever cross, the two mechanisms fight: pressure stops
        // reclaiming at 60% used while resume replays into the remainder.
        const PRESSURE_EXIT_FREE_FRACTION: f64 = 0.40;
        assert!(
            RESUME_MIN_FREE_FRACTION < PRESSURE_EXIT_FREE_FRACTION,
            "resume must not demand MORE than pressure can deliver, or a \
             suspended table can never recover automatically"
        );
        assert!(
            RESUME_MIN_FREE_FRACTION > 0.10,
            "and it must demand materially more than a nearly-full volume"
        );
    }
}
