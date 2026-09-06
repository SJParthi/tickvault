//! Settles what the Dhan WebSocket `volume` field actually MEANS, using data
//! we already store — no new API call, no new table, no rule-file change.
//!
//! ## The question, and why it is the most important open one
//!
//! The whole volume-ranked depth steering rests on one premise: that the
//! `volume` field in the Quote and Full packets is a RUNNING DAY TOTAL. If it
//! is instead a per-packet quantity, then `volume_leaderboard`'s "only ever
//! up" gate refuses nearly every observation and the ranking never moves.
//!
//! **No Dhan document states which it is.** A sweep of 28 official docs and 21
//! reference files found the packet layouts describe bytes 23-26 as bare
//! `Volume` and nothing more, while the NEIGHBOURING fields are qualified
//! ("Last Traded Quantity", "Total Sell Quantity"). Greps for cumulative /
//! since / running total / reset / monotonic return ZERO hits.
//!
//! Two places in this repository already ASSERT cumulative —
//! `tick_persistence`'s column doc says "Cumulative day volume", and the
//! leaderboard's gate depends on it. Neither is evidence: they are the same
//! assumption written down twice.
//!
//! ## The test
//!
//! The per-minute option-chain REST leg already stores a per-contract volume
//! that Dhan's OWN documentation calls "Today's traded volume". So for any
//! contract present in both `ticks` and `rest_option_chain_1m`, the two
//! hypotheses make predictions that differ by ORDERS OF MAGNITUDE:
//!
//! | If `volume` is | then | and |
//! |---|---|---|
//! | a running day total | `max(ticks.volume)` ≈ the REST day total | `sum` is ~N/2 times too big |
//! | a per-packet quantity | `sum(ticks.volume)` ≈ the REST day total | `max` is one trade, tiny |
//!
//! Whichever is closer wins that contract's vote. With hundreds of contracts
//! the answer is not a judgement call.
//!
//! ## What this module deliberately does NOT do
//!
//! It does not call `/marketfeed/quote`. That endpoint is FORBIDDEN by
//! `no-rest-except-live-feed-2026-06-27.md` §11.3 and re-adding a caller would
//! need a fresh dated operator quote. It is also not needed: the chain leg
//! already stores the oracle, so the cheaper route is also the one that
//! requires no rule change. Recorded because the expensive route was the
//! obvious one and was rejected on purpose.
//!
//! It also renders NO verdict on thin evidence. Every threshold below has a
//! refusal arm, and `Inconclusive` is a first-class outcome — a probe that
//! always answers is a probe that will eventually answer wrongly.

/// Per-contract evidence, one row per contract that appears in both sources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractVolumeEvidence {
    /// The option contract's own security id.
    pub security_id: i64,
    /// Segment, kept beside the id per I-P1-11 — the bare id is reused.
    pub segment: String,
    /// `max(volume)` across the contract's ticks for the day.
    pub ws_max: i64,
    /// `sum(volume)` across the contract's ticks for the day.
    pub ws_sum: i64,
    /// The REST day total: the last `rest_option_chain_1m.volume` for this
    /// contract, which Dhan documents as "Today's traded volume".
    pub rest_day_total: i64,
}

/// What the packet field turned out to mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeSemantics {
    /// A running day total — the premise the ranking is built on.
    Cumulative,
    /// A per-packet quantity — which would mean the monotonicity gate is
    /// refusing nearly everything and the ranking is effectively frozen.
    PerPacketDelta,
    /// Not enough agreeing evidence. Deliberately a real outcome.
    Inconclusive,
}

impl VolumeSemantics {
    /// Stable label for logs and reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cumulative => "cumulative",
            Self::PerPacketDelta => "per_packet_delta",
            Self::Inconclusive => "inconclusive",
        }
    }
}

/// The full result, carrying the counts so a reader can check the verdict
/// rather than trust it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeSemanticsVerdict {
    /// The answer.
    pub semantics: VolumeSemantics,
    /// Why, in one phrase — including why a refusal was refused.
    pub reason: &'static str,
    /// Contracts that had usable evidence on both sides.
    pub contracts_voting: usize,
    /// Votes for the running-day-total reading.
    pub votes_cumulative: usize,
    /// Votes for the per-packet reading.
    pub votes_delta: usize,
    /// Contracts where NEITHER hypothesis landed within tolerance. A large
    /// share here means something else is wrong and no verdict is safe.
    pub contracts_unmatched: usize,
}

/// A contract's vote counts only if the winning hypothesis lands within this
/// relative error. 20% is loose on purpose: the two hypotheses differ by
/// orders of magnitude, so the band never has to be tight to separate them,
/// and a tight band would reject honest partial capture.
pub const MAX_ACCEPTED_ERROR_BP: i64 = 2_000;

/// Below this many voting contracts there is no verdict. One contract can
/// agree with anything by accident; twenty cannot.
pub const MIN_CONTRACTS_FOR_VERDICT: usize = 20;

/// The winning side must hold at least this share of votes, in basis points.
pub const REQUIRED_SUPERMAJORITY_BP: i64 = 8_000;

/// If more than this share of contracts match NEITHER hypothesis, the data
/// is describing something this probe does not model, and it says so instead
/// of picking the less-wrong of two wrong answers.
pub const MAX_UNMATCHED_SHARE_BP: i64 = 5_000;

/// Relative error of `observed` against `expected`, in basis points.
/// Integer arithmetic throughout — this repository compares prices in paise
/// and never on a float epsilon, and a verdict deserves the same.
///
/// Returns `None` when `expected` is zero: a zero denominator says nothing,
/// and returning a huge number instead would let a contract that never
/// traded outvote one that did.
#[must_use]
pub fn relative_error_bp(observed: i64, expected: i64) -> Option<i64> {
    if expected <= 0 {
        return None;
    }
    let diff = observed.saturating_sub(expected).saturating_abs();
    diff.checked_mul(10_000).map(|scaled| scaled / expected)
}

/// Which hypothesis this one contract supports, if either.
///
/// `None` means the contract abstains — no REST total, no ticks, or neither
/// hypothesis within [`MAX_ACCEPTED_ERROR_BP`].
#[must_use]
pub fn vote_for(evidence: &ContractVolumeEvidence) -> Option<VolumeSemantics> {
    if evidence.rest_day_total <= 0 || evidence.ws_max <= 0 {
        return None;
    }
    let cumulative_err = relative_error_bp(evidence.ws_max, evidence.rest_day_total)?;
    let delta_err = relative_error_bp(evidence.ws_sum, evidence.rest_day_total)?;

    // A single observation makes max == sum, so the contract cannot tell the
    // hypotheses apart and must abstain rather than vote for whichever the
    // comparison happens to order first.
    if evidence.ws_max == evidence.ws_sum {
        return None;
    }

    let (winner, winning_err) = if cumulative_err <= delta_err {
        (VolumeSemantics::Cumulative, cumulative_err)
    } else {
        (VolumeSemantics::PerPacketDelta, delta_err)
    };
    if winning_err > MAX_ACCEPTED_ERROR_BP {
        return None;
    }
    Some(winner)
}

/// Renders the verdict over every contract's evidence. Pure — no I/O, so the
/// decision rule is testable without a live database.
#[must_use]
pub fn classify_volume_semantics(evidence: &[ContractVolumeEvidence]) -> VolumeSemanticsVerdict {
    let mut votes_cumulative = 0usize;
    let mut votes_delta = 0usize;
    let mut unmatched = 0usize;

    for e in evidence {
        match vote_for(e) {
            Some(VolumeSemantics::Cumulative) => votes_cumulative += 1,
            Some(VolumeSemantics::PerPacketDelta) => votes_delta += 1,
            // An abstention on a contract that HAD both sides is an
            // unmatched contract; one missing a side is simply not evidence.
            Some(VolumeSemantics::Inconclusive) | None => {
                if e.rest_day_total > 0 && e.ws_max > 0 && e.ws_max != e.ws_sum {
                    unmatched += 1;
                }
            }
        }
    }

    let voting = votes_cumulative + votes_delta;
    let base = VolumeSemanticsVerdict {
        semantics: VolumeSemantics::Inconclusive,
        reason: "",
        contracts_voting: voting,
        votes_cumulative,
        votes_delta,
        contracts_unmatched: unmatched,
    };

    if voting < MIN_CONTRACTS_FOR_VERDICT {
        return VolumeSemanticsVerdict {
            reason: "too few contracts had usable evidence on both sides",
            ..base
        };
    }

    // Checked BEFORE the supermajority: if most contracts match neither
    // hypothesis, a lopsided vote among the remainder is the less-wrong of
    // two wrong answers, not an answer.
    let considered = voting + unmatched;
    if considered > 0 {
        let unmatched_share_bp = i64::try_from(unmatched)
            .unwrap_or(i64::MAX)
            .saturating_mul(10_000)
            / i64::try_from(considered).unwrap_or(1);
        if unmatched_share_bp > MAX_UNMATCHED_SHARE_BP {
            return VolumeSemanticsVerdict {
                reason: "most contracts matched NEITHER hypothesis — the data is \
                         describing something this probe does not model",
                ..base
            };
        }
    }

    let voting_i64 = i64::try_from(voting).unwrap_or(1).max(1);
    let cumulative_share_bp = i64::try_from(votes_cumulative)
        .unwrap_or(0)
        .saturating_mul(10_000)
        / voting_i64;
    let delta_share_bp = i64::try_from(votes_delta)
        .unwrap_or(0)
        .saturating_mul(10_000)
        / voting_i64;

    if cumulative_share_bp >= REQUIRED_SUPERMAJORITY_BP {
        VolumeSemanticsVerdict {
            semantics: VolumeSemantics::Cumulative,
            reason: "max(ws volume) matches the vendor's own day total",
            ..base
        }
    } else if delta_share_bp >= REQUIRED_SUPERMAJORITY_BP {
        VolumeSemanticsVerdict {
            semantics: VolumeSemantics::PerPacketDelta,
            reason: "sum(ws volume) matches the vendor's own day total — the \
                     monotonicity gate is refusing nearly everything",
            ..base
        }
    } else {
        VolumeSemanticsVerdict {
            reason: "the contracts disagree — no supermajority either way",
            ..base
        }
    }
}

/// Per-contract `max`/`sum` of the WS-sourced volume for one IST trading day.
///
/// `segment` is selected alongside `security_id` because the bare id is
/// reused across segments (I-P1-11) — grouping on the id alone would merge
/// two different instruments into one contract's evidence.
#[must_use]
pub fn ws_volume_evidence_sql(trading_date_ist: &str) -> String {
    format!(
        "SELECT security_id, segment, max(volume) AS ws_max, sum(volume) AS ws_sum \
         FROM ticks \
         WHERE feed = 'dhan' AND ts IN '{trading_date_ist}' AND volume > 0 \
         GROUP BY security_id, segment"
    )
}

/// The vendor's own day total per contract — the LAST chain row of the day,
/// whose `volume` Dhan documents as "Today's traded volume".
///
/// `contract_security_id > 0` mirrors `dhan_depth_universe`'s existing
/// filter: the parser defaults that field to 0 when absent, and a zero id
/// would join against instrument 0.
#[must_use]
pub fn rest_day_total_sql(trading_date_ist: &str) -> String {
    format!(
        "SELECT contract_security_id, volume AS rest_day_total \
         FROM rest_option_chain_1m \
         WHERE feed = 'dhan' AND ts IN '{trading_date_ist}' AND contract_security_id > 0 \
         LATEST ON ts PARTITION BY contract_security_id"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds evidence for a contract whose WS ticks are a running day total:
    /// the last tick carries the whole day, so `max` IS the day total and
    /// `sum` is the sum of every running total ever seen (far larger).
    fn cumulative_shaped(security_id: i64, day_total: i64, ticks: i64) -> ContractVolumeEvidence {
        // A running total climbing in equal steps to `day_total` over `ticks`
        // observations: max == day_total, sum == day_total * (ticks + 1) / 2.
        let sum = day_total.saturating_mul(ticks + 1) / 2;
        ContractVolumeEvidence {
            security_id,
            segment: "NSE_FNO".to_string(),
            ws_max: day_total,
            ws_sum: sum,
            rest_day_total: day_total,
        }
    }

    /// Builds evidence for a contract whose WS ticks are per-packet quantities:
    /// the pieces SUM to the day total and any single one is tiny.
    fn delta_shaped(security_id: i64, day_total: i64, ticks: i64) -> ContractVolumeEvidence {
        let per_tick = (day_total / ticks).max(1);
        ContractVolumeEvidence {
            security_id,
            segment: "NSE_FNO".to_string(),
            ws_max: per_tick,
            ws_sum: per_tick.saturating_mul(ticks),
            rest_day_total: per_tick.saturating_mul(ticks),
        }
    }

    #[test]
    fn relative_error_bp_is_zero_on_an_exact_match() {
        assert_eq!(relative_error_bp(1_000, 1_000), Some(0));
    }

    #[test]
    fn relative_error_bp_is_symmetric_above_and_below() {
        // 10% over and 10% under both read as 1,000 bp — the probe cares how
        // FAR a hypothesis is, never which side it missed on.
        assert_eq!(relative_error_bp(1_100, 1_000), Some(1_000));
        assert_eq!(relative_error_bp(900, 1_000), Some(1_000));
    }

    #[test]
    fn relative_error_bp_refuses_a_zero_or_negative_denominator() {
        // A contract with no REST total says nothing. Returning a huge error
        // instead would let it outvote a contract that actually traded.
        assert_eq!(relative_error_bp(5, 0), None);
        assert_eq!(relative_error_bp(5, -1), None);
    }

    #[test]
    fn relative_error_bp_never_overflows_on_a_huge_observation() {
        // i64::MAX * 10_000 overflows; the checked_mul arm must return None
        // rather than panic in release-with-overflow-checks (this workspace
        // builds release with overflow-checks = true).
        assert_eq!(relative_error_bp(i64::MAX, 1), None);
    }

    #[test]
    fn vote_for_picks_cumulative_when_max_matches_the_day_total() {
        let e = cumulative_shaped(1, 100_000, 400);
        assert_eq!(vote_for(&e), Some(VolumeSemantics::Cumulative));
    }

    #[test]
    fn vote_for_picks_delta_when_sum_matches_the_day_total() {
        let e = delta_shaped(2, 100_000, 400);
        assert_eq!(vote_for(&e), Some(VolumeSemantics::PerPacketDelta));
    }

    #[test]
    fn vote_for_abstains_when_max_equals_sum() {
        // One observation cannot distinguish the hypotheses: max == sum makes
        // both errors identical, so the comparison would silently hand the
        // vote to whichever branch is written first. It must abstain instead.
        let e = ContractVolumeEvidence {
            security_id: 3,
            segment: "NSE_FNO".to_string(),
            ws_max: 500,
            ws_sum: 500,
            rest_day_total: 500,
        };
        assert_eq!(vote_for(&e), None);
    }

    #[test]
    fn vote_for_abstains_when_neither_hypothesis_is_close() {
        // Day total wildly apart from both max and sum: something else is
        // wrong with this contract and it must not vote at all.
        let e = ContractVolumeEvidence {
            security_id: 4,
            segment: "NSE_FNO".to_string(),
            ws_max: 10,
            ws_sum: 90,
            rest_day_total: 1_000_000,
        };
        assert_eq!(vote_for(&e), None);
    }

    #[test]
    fn vote_for_abstains_without_a_rest_total_or_without_ticks() {
        let no_rest = ContractVolumeEvidence {
            security_id: 5,
            segment: "NSE_FNO".to_string(),
            ws_max: 100,
            ws_sum: 900,
            rest_day_total: 0,
        };
        let no_ticks = ContractVolumeEvidence {
            security_id: 6,
            segment: "NSE_FNO".to_string(),
            ws_max: 0,
            ws_sum: 0,
            rest_day_total: 900,
        };
        assert_eq!(vote_for(&no_rest), None);
        assert_eq!(vote_for(&no_ticks), None);
    }

    #[test]
    fn classify_volume_semantics_reads_cumulative_from_a_clean_supermajority() {
        let evidence: Vec<_> = (0..40)
            .map(|i| cumulative_shaped(i, 50_000 + i * 137, 300))
            .collect();
        let verdict = classify_volume_semantics(&evidence);
        assert_eq!(verdict.semantics, VolumeSemantics::Cumulative);
        assert_eq!(verdict.votes_cumulative, 40);
        assert_eq!(verdict.votes_delta, 0);
        assert_eq!(verdict.contracts_unmatched, 0);
    }

    #[test]
    fn classify_volume_semantics_reads_delta_from_a_clean_supermajority() {
        let evidence: Vec<_> = (0..40)
            .map(|i| delta_shaped(i, 50_000 + i * 137, 300))
            .collect();
        let verdict = classify_volume_semantics(&evidence);
        assert_eq!(verdict.semantics, VolumeSemantics::PerPacketDelta);
        assert_eq!(verdict.votes_delta, 40);
        assert_eq!(verdict.votes_cumulative, 0);
    }

    #[test]
    fn classify_volume_semantics_refuses_a_verdict_on_too_few_contracts() {
        // Unanimous, and still refused: one contract can agree with anything
        // by accident. This is the arm that keeps a thin day from producing a
        // confident wrong answer.
        let evidence: Vec<_> = (0..(MIN_CONTRACTS_FOR_VERDICT as i64 - 1))
            .map(|i| cumulative_shaped(i, 50_000, 300))
            .collect();
        let verdict = classify_volume_semantics(&evidence);
        assert_eq!(verdict.semantics, VolumeSemantics::Inconclusive);
        assert!(verdict.reason.contains("too few"));
    }

    #[test]
    fn classify_volume_semantics_refuses_a_verdict_on_a_split_vote() {
        let mut evidence: Vec<_> = (0..30).map(|i| cumulative_shaped(i, 50_000, 300)).collect();
        evidence.extend((30..60).map(|i| delta_shaped(i, 50_000, 300)));
        let verdict = classify_volume_semantics(&evidence);
        assert_eq!(verdict.semantics, VolumeSemantics::Inconclusive);
        assert!(verdict.reason.contains("disagree"));
    }

    #[test]
    fn classify_volume_semantics_refuses_when_most_contracts_match_neither() {
        // 25 clean cumulative votes would be a supermajority on their own —
        // but 60 contracts match NEITHER hypothesis, so the population is
        // describing something this probe does not model and a lopsided vote
        // among the remainder is the less-wrong of two wrong answers.
        let mut evidence: Vec<_> = (0..25).map(|i| cumulative_shaped(i, 50_000, 300)).collect();
        evidence.extend((25..85).map(|i| ContractVolumeEvidence {
            security_id: i,
            segment: "NSE_FNO".to_string(),
            ws_max: 10,
            ws_sum: 90,
            rest_day_total: 1_000_000,
        }));
        let verdict = classify_volume_semantics(&evidence);
        assert_eq!(verdict.semantics, VolumeSemantics::Inconclusive);
        assert!(verdict.reason.contains("NEITHER"));
        assert_eq!(verdict.contracts_unmatched, 60);
        assert_eq!(verdict.contracts_voting, 25);
    }

    #[test]
    fn classify_volume_semantics_returns_inconclusive_on_empty_evidence() {
        let verdict = classify_volume_semantics(&[]);
        assert_eq!(verdict.semantics, VolumeSemantics::Inconclusive);
        assert_eq!(verdict.contracts_voting, 0);
        assert_eq!(verdict.contracts_unmatched, 0);
    }

    #[test]
    fn ws_volume_evidence_sql_groups_on_the_composite_key_not_the_bare_id() {
        // I-P1-11: the bare security_id is reused across segments, so a
        // GROUP BY on it alone would merge two instruments into one vote.
        let sql = ws_volume_evidence_sql("2026-09-08");
        assert!(sql.contains("GROUP BY security_id, segment"), "{sql}");
        assert!(sql.contains("max(volume)"), "{sql}");
        assert!(sql.contains("sum(volume)"), "{sql}");
        assert!(sql.contains("2026-09-08"), "{sql}");
        assert!(sql.contains("feed = 'dhan'"), "{sql}");
    }

    #[test]
    fn rest_day_total_sql_takes_the_last_row_and_skips_the_zero_id_sentinel() {
        // contract_security_id defaults to 0 when the vendor omits the field;
        // joining on it would attribute a real contract's volume to id 0.
        let sql = rest_day_total_sql("2026-09-08");
        assert!(sql.contains("contract_security_id > 0"), "{sql}");
        assert!(
            sql.contains("LATEST ON ts PARTITION BY contract_security_id"),
            "{sql}"
        );
        assert!(sql.contains("rest_option_chain_1m"), "{sql}");
    }

    #[test]
    fn neither_sql_touches_the_forbidden_marketfeed_endpoint() {
        // `/marketfeed/quote` stays FORBIDDEN by
        // no-rest-except-live-feed-2026-06-27.md §11.3. This probe answers the
        // question from data already on disk precisely so no rule has to move.
        for sql in [
            ws_volume_evidence_sql("2026-09-08"),
            rest_day_total_sql("2026-09-08"),
        ] {
            assert!(!sql.contains("marketfeed"), "{sql}");
        }
    }

    #[test]
    fn as_str_labels_are_stable_and_distinct_for_every_variant() {
        assert_eq!(VolumeSemantics::Cumulative.as_str(), "cumulative");
        assert_eq!(VolumeSemantics::PerPacketDelta.as_str(), "per_packet_delta");
        assert_eq!(VolumeSemantics::Inconclusive.as_str(), "inconclusive");
    }

    #[test]
    fn the_thresholds_are_ordered_so_no_arm_is_unreachable() {
        // A supermajority bar at or below half would make the split-vote arm
        // dead code; an unmatched ceiling at 100% would make the
        // matches-neither arm dead code. Both would look like passing tests.
        assert!(REQUIRED_SUPERMAJORITY_BP > 5_000);
        assert!(REQUIRED_SUPERMAJORITY_BP <= 10_000);
        assert!(MAX_UNMATCHED_SHARE_BP > 0 && MAX_UNMATCHED_SHARE_BP < 10_000);
        assert!(MAX_ACCEPTED_ERROR_BP > 0 && MAX_ACCEPTED_ERROR_BP < 10_000);
        assert!(MIN_CONTRACTS_FOR_VERDICT >= 2);
    }
}
