//! Depth-200 ATM tracking: which four contracts the 200-level sockets carry,
//! recomputed every minute from data already on disk.
//!
//! # What this is for
//!
//! Operator, 2026-08-26: *"for evry one minute alwyas espeiclaly for dpeth 200
//! … nifty atm ce atm pe always … even for bancknfity atm ce atm pe also"*,
//! and then *"as fo now dont need to conside the fifth one dude jsut go aheaf
//! with 4 aloe"*.
//!
//! The depth-200 set used to be chosen ONCE, at attach, and never revisited —
//! `dhan_feed_stack` skips re-selection after `depth_done` on the recorded
//! grounds that re-running the queries "buys nothing" for an already
//! subscribed set. That is true of a set chosen by IDENTITY and false of a set
//! chosen by ATM: a strike that is at-the-money at 09:10 is not at-the-money
//! at 14:00 if the index moved, so the boot-time choice decays all session
//! while every health signal keeps reading green.
//!
//! # The four-word test (`z-plus-defense-doctrine.md`)
//!
//! | Word | How this satisfies it |
//! |---|---|
//! | **Common** | one code path, no `cfg` divergence; the same tracker runs on a dev box and on the prod instance, driven by the same chain rows |
//! | **Runtime** | the tracked underlyings and the hysteresis are DATA ([`Depth200AtmConfig`]), not literals — changing them needs no code change |
//! | **Dynamic** | the set reshapes on observed spot; that is the entire point |
//! | **Scalable** | O(strikes) per underlying per minute with ZERO allocation in the scan, and the tracker's map is bounded by the configured underlying list, not by anything a vendor controls |
//! | **Incremental** | pure and self-contained: it decides, it does not act. Wiring the decisions to the socket layer is a separate, reviewable step |
//!
//! # Why the input costs nothing
//!
//! The option-chain leg already fetches NIFTY and BANKNIFTY every minute and
//! persists `option_chain_1m` with each leg's `contract_security_id` and the
//! underlying spot. ATM is therefore derivable from rows we already have: no
//! new REST call, no new rate-limit budget, no new failure mode of its own.
//!
//! # The rule the whole module is built around
//!
//! **A bad or missing input never costs a live subscription.** Every refusal
//! path below keeps the current set rather than unsubscribing into nothing. A
//! socket carrying a slightly stale strike is strictly better than a socket
//! carrying nothing, and that asymmetry decides every edge case here.

use std::collections::BTreeMap;

/// Underlyings whose ATM CE/PE pairs occupy the depth-200 sockets.
///
/// Ordered, and the order is load-bearing when only one pair can fit.
pub const DEPTH_200_ATM_UNDERLYINGS: [&str; 2] = ["NIFTY", "BANKNIFTY"];

/// Sockets the ATM set occupies: two underlyings times a CE/PE pair.
///
/// Deliberately FOUR, not the vendor ceiling of five — see
/// `dhan_depth_universe::DEPTH_200_MAX_SOCKETS` for the operator instruction
/// and the cost of the idle fifth.
pub const DEPTH_200_ATM_SOCKETS: usize = DEPTH_200_ATM_UNDERLYINGS.len() * 2;

/// How much nearer the challenger must be before a switch is allowed, as a
/// fraction of the chain's strike spacing.
///
/// # Why a deadband exists at all
///
/// Spot spends real time near the midpoint between two strikes, and that is
/// exactly when the market is most active. Pure nearest-strike would flip the
/// subscription back and forth every minute at precisely the moment the book
/// matters most, and each flip is an unsubscribe/subscribe with a gap in
/// coverage. 25% of one strike spacing is wide enough that ordinary noise
/// around the midpoint cannot cross it, and narrow enough that a genuine move
/// of a quarter-strike still switches.
pub const ATM_SWITCH_MARGIN_FRACTION: f64 = 0.25;

/// Consecutive observations the challenger must win before the switch fires.
///
/// The margin above stops midpoint chatter; this stops a single anomalous
/// snapshot — one bad spot print, one partially-populated chain — from moving
/// a live subscription. Two is the smallest value that requires corroboration.
pub const ATM_SWITCH_CONFIRM_OBSERVATIONS: u32 = 2;

/// One strike's pair as the chain reports it, already resolved to the two
/// contract ids depth-200 would subscribe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StrikePair {
    /// Strike in paise, so equality is exact and two chain rows for one strike
    /// can never fail to group through float drift.
    pub strike_paise: i64,
    /// The CE contract's own Dhan `security_id` — the thing depth subscribes.
    pub ce_security_id: i64,
    /// The PE contract's own Dhan `security_id`.
    pub pe_security_id: i64,
}

/// One underlying's view for one minute.
#[derive(Debug, Clone)]
pub struct ChainMinute<'a> {
    /// Canonical underlying symbol (`NIFTY`, `BANKNIFTY`).
    pub underlying: &'a str,
    /// Underlying spot at this minute.
    pub spot: f64,
    /// Every strike in the snapshot that carries BOTH legs.
    pub pairs: &'a [StrikePair],
}

/// Why a minute produced no switch. Every variant is COUNTED at the call site
/// rather than swallowed: "nothing changed" and "we could not tell" are
/// different states and must not share a silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoSwitch {
    /// The subscribed strike is still ATM. The overwhelmingly common case.
    AlreadyAtm,
    /// A challenger leads but has not yet cleared the margin.
    WithinMargin,
    /// A challenger leads and clears the margin, but has not yet been
    /// confirmed for [`ATM_SWITCH_CONFIRM_OBSERVATIONS`] observations.
    AwaitingConfirmation,
    /// Spot was absent, non-finite or non-positive.
    UnusableSpot,
    /// The snapshot carried no usable strike pair.
    NoUsablePairs,
    /// This underlying is not tracked by depth-200.
    UntrackedUnderlying,
}

/// A decided change for one underlying: unsubscribe `from`, subscribe `to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtmSwitch {
    /// Index into [`DEPTH_200_ATM_UNDERLYINGS`] — the socket pair to act on.
    pub underlying_index: usize,
    /// The pair currently subscribed, or `None` on the first adoption.
    pub from: Option<StrikePair>,
    /// The pair to subscribe.
    pub to: StrikePair,
    /// Why the switch fired, for the operator-facing log line.
    pub reason: SwitchReason,
}

/// The distinguishable causes of a switch. Separated because they have
/// different urgency and different diagnostic meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchReason {
    /// No pair was subscribed yet. Adopted immediately, no hysteresis — there
    /// is no live subscription to protect and waiting would leave the socket
    /// empty for no benefit.
    FirstAdoption,
    /// Spot moved far enough for long enough. The ordinary case.
    SpotMoved,
    /// The subscribed strike is no longer in the chain at all — an expiry
    /// roll, or the vendor dropping it. Switched IMMEDIATELY, bypassing the
    /// margin and the confirmation count, because the alternative is holding a
    /// subscription to a contract that no longer exists.
    SubscribedStrikeVanished,
    /// The strike is unchanged but its contract id is not. Derivative ids are
    /// documented as unstable across days, so the SUBSCRIPTION KEY changed
    /// even though the price did not. Missing this would leave the socket on a
    /// dead id while every strike-level check reported agreement.
    ContractIdChanged,
}

/// Runtime-tunable knobs. Data, not literals, so the behaviour is changeable
/// without a code change (the "Runtime" leg of the four-word test).
#[derive(Debug, Clone, Copy)]
pub struct Depth200AtmConfig {
    /// See [`ATM_SWITCH_MARGIN_FRACTION`]. Clamped to `[0.0, 1.0]` on use: a
    /// negative margin would switch on noise and one above a full spacing
    /// could never switch at all, and both are worse than any value in range.
    pub switch_margin_fraction: f64,
    /// See [`ATM_SWITCH_CONFIRM_OBSERVATIONS`]. Clamped to at least 1.
    pub confirm_observations: u32,
}

impl Default for Depth200AtmConfig {
    fn default() -> Self {
        Self {
            switch_margin_fraction: ATM_SWITCH_MARGIN_FRACTION,
            confirm_observations: ATM_SWITCH_CONFIRM_OBSERVATIONS,
        }
    }
}

/// Per-underlying state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TrackedPair {
    current: StrikePair,
    /// The challenger being confirmed, and how many observations it has won.
    challenger: Option<(i64, u32)>,
}

/// Decides which contracts the depth-200 sockets should carry, minute by
/// minute. Pure: it returns decisions and never performs I/O.
#[derive(Debug, Clone, Default)]
pub struct Depth200AtmTracker {
    /// Keyed by index into [`DEPTH_200_ATM_UNDERLYINGS`], so the map is
    /// bounded by that array and can never grow from vendor input — the
    /// unbounded-per-entity-map class CLAUDE.md records five times.
    state: BTreeMap<usize, TrackedPair>,
    config: Depth200AtmConfig,
}

/// Index of `underlying` in [`DEPTH_200_ATM_UNDERLYINGS`], case-insensitive.
#[must_use]
pub fn tracked_underlying_index(underlying: &str) -> Option<usize> {
    let trimmed = underlying.trim();
    DEPTH_200_ATM_UNDERLYINGS
        .iter()
        .position(|u| u.eq_ignore_ascii_case(trimmed))
}

/// A strike is usable only if it is a real, positive, finite price and both
/// its legs carry a non-zero contract id.
///
/// Zero is the documented ABSENT sentinel for `contract_security_id` — the
/// chain parser defaults the field to 0 when the vendor omits it — so
/// subscribing a zero would ask the socket for instrument 0 and look
/// perfectly healthy while carrying nothing.
#[must_use]
fn pair_is_usable(pair: &StrikePair) -> bool {
    pair.strike_paise > 0
        && pair.ce_security_id > 0
        && pair.pe_security_id > 0
        // A vendor row giving both legs the same id is corrupt: one socket
        // would shadow the other and the pair would silently be a single leg.
        && pair.ce_security_id != pair.pe_security_id
}

/// Median gap between consecutive distinct strikes, in paise.
///
/// The MEDIAN and not the minimum, because real chains carry irregular gaps
/// (a missing strike doubles one gap; some chains tighten spacing near ATM),
/// and a single anomalous gap must not redefine the deadband for the whole
/// chain. `None` when fewer than two distinct strikes exist — a spacing cannot
/// be inferred from one strike, and inventing one would fabricate a deadband.
fn median_spacing_paise(sorted_strikes: &[i64]) -> Option<i64> {
    if sorted_strikes.len() < 2 {
        return None;
    }
    let mut gaps: Vec<i64> = sorted_strikes
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]))
        .filter(|g| *g > 0)
        .collect();
    if gaps.is_empty() {
        return None;
    }
    gaps.sort_unstable();
    Some(gaps[gaps.len() / 2])
}

impl Depth200AtmTracker {
    #[must_use]
    pub fn new(config: Depth200AtmConfig) -> Self {
        Self {
            state: BTreeMap::new(),
            config,
        }
    }

    /// The pair currently subscribed for `underlying_index`, if any.
    #[must_use]
    pub fn current(&self, underlying_index: usize) -> Option<StrikePair> {
        self.state.get(&underlying_index).map(|t| t.current)
    }

    /// How many underlyings are tracked. Bounded by
    /// [`DEPTH_200_ATM_UNDERLYINGS`]; exposed so the bound is assertable
    /// rather than merely promised.
    #[must_use]
    pub fn tracked_len(&self) -> usize {
        self.state.len()
    }

    /// Feed one underlying's minute. Returns the switch to perform, or the
    /// reason none was.
    ///
    /// O(strikes) with one allocation for the spacing calculation and none in
    /// the nearest-strike scan.
    pub fn observe(&mut self, minute: &ChainMinute<'_>) -> Result<AtmSwitch, NoSwitch> {
        let Some(idx) = tracked_underlying_index(minute.underlying) else {
            return Err(NoSwitch::UntrackedUnderlying);
        };
        if !minute.spot.is_finite() || minute.spot <= 0.0 {
            return Err(NoSwitch::UnusableSpot);
        }

        // Nearest usable strike to spot. Ties break to the LOWER strike, which
        // is arbitrary but must be DETERMINISTIC: two equidistant strikes that
        // resolved differently on alternate minutes would flip the
        // subscription forever, and the deadband cannot help because the
        // distances are exactly equal.
        let spot_paise = spot_to_paise(minute.spot);
        let mut best: Option<(i64, StrikePair)> = None;
        let mut strikes: Vec<i64> = Vec::with_capacity(minute.pairs.len());
        for pair in minute.pairs {
            if !pair_is_usable(pair) {
                continue;
            }
            strikes.push(pair.strike_paise);
            let distance = pair.strike_paise.saturating_sub(spot_paise).abs();
            match best {
                Some((best_distance, best_pair))
                    if best_distance < distance
                        || (best_distance == distance
                            && best_pair.strike_paise <= pair.strike_paise) => {}
                _ => best = Some((distance, *pair)),
            }
        }
        let Some((challenger_distance, challenger)) = best else {
            return Err(NoSwitch::NoUsablePairs);
        };
        strikes.sort_unstable();
        strikes.dedup();

        let Some(tracked) = self.state.get(&idx).copied() else {
            // Nothing subscribed yet: adopt immediately. There is no live
            // subscription to protect, and hysteresis here would only leave
            // the socket empty for two more minutes.
            self.state.insert(
                idx,
                TrackedPair {
                    current: challenger,
                    challenger: None,
                },
            );
            return Ok(AtmSwitch {
                underlying_index: idx,
                from: None,
                to: challenger,
                reason: SwitchReason::FirstAdoption,
            });
        };

        // The subscribed strike is gone from the chain — expiry roll, or the
        // vendor dropped it. Holding a subscription to a contract that no
        // longer exists is strictly worse than switching now, so this bypasses
        // both the margin and the confirmation count.
        let current_still_listed = strikes.binary_search(&tracked.current.strike_paise).is_ok();
        if !current_still_listed {
            return Ok(self.commit(
                idx,
                tracked,
                challenger,
                SwitchReason::SubscribedStrikeVanished,
            ));
        }

        // Same strike, different contract id. The SUBSCRIPTION KEY is the
        // security_id, not the strike, and Dhan documents derivative ids as
        // unstable across days. Comparing strikes alone would leave the socket
        // on a dead id while every price-level check agreed.
        if challenger.strike_paise == tracked.current.strike_paise {
            if challenger.ce_security_id != tracked.current.ce_security_id
                || challenger.pe_security_id != tracked.current.pe_security_id
            {
                return Ok(self.commit(idx, tracked, challenger, SwitchReason::ContractIdChanged));
            }
            // Genuinely unchanged: clear any half-confirmed challenger so a
            // later move starts its count fresh rather than inheriting one.
            self.state.insert(
                idx,
                TrackedPair {
                    current: tracked.current,
                    challenger: None,
                },
            );
            return Err(NoSwitch::AlreadyAtm);
        }

        // A different strike now leads. It must beat the subscribed one by
        // more than the deadband before it is even a candidate.
        let current_distance = tracked
            .current
            .strike_paise
            .saturating_sub(spot_paise)
            .abs();
        let Some(spacing) = median_spacing_paise(&strikes) else {
            // One strike in the chain and it is not the subscribed one. No
            // spacing can be inferred, so no deadband can be applied; the
            // vanished-strike branch above already handles the case where the
            // subscribed strike is absent, so reaching here means the chain
            // shrank to a single strike that we hold. Keep what we have.
            return Err(NoSwitch::WithinMargin);
        };
        let margin = margin_paise(spacing, self.config.switch_margin_fraction);
        if current_distance.saturating_sub(challenger_distance) <= margin {
            self.state.insert(
                idx,
                TrackedPair {
                    current: tracked.current,
                    challenger: None,
                },
            );
            return Err(NoSwitch::WithinMargin);
        }

        // Clears the margin. Now it must also hold for N observations, so one
        // anomalous spot print cannot move a live subscription.
        let needed = self.config.confirm_observations.max(1);
        let wins = match tracked.challenger {
            Some((strike, count)) if strike == challenger.strike_paise => count.saturating_add(1),
            _ => 1,
        };
        if wins < needed {
            self.state.insert(
                idx,
                TrackedPair {
                    current: tracked.current,
                    challenger: Some((challenger.strike_paise, wins)),
                },
            );
            return Err(NoSwitch::AwaitingConfirmation);
        }
        Ok(self.commit(idx, tracked, challenger, SwitchReason::SpotMoved))
    }

    fn commit(
        &mut self,
        idx: usize,
        tracked: TrackedPair,
        to: StrikePair,
        reason: SwitchReason,
    ) -> AtmSwitch {
        self.state.insert(
            idx,
            TrackedPair {
                current: to,
                challenger: None,
            },
        );
        AtmSwitch {
            underlying_index: idx,
            from: Some(tracked.current),
            to,
            reason,
        }
    }
}

/// Spot to paise, saturating. A spot beyond `i64` paise is not a real index
/// level; saturating keeps the comparison total instead of panicking or
/// wrapping into a negative that would rank first.
fn spot_to_paise(spot: f64) -> i64 {
    let paise = (spot * 100.0).round();
    if paise.is_finite() {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "saturating by design; a spot beyond i64 paise is not a real index level"
        )]
        {
            paise as i64
        }
    } else {
        i64::MAX
    }
}

/// Deadband in paise, from the chain spacing and the configured fraction.
///
/// The fraction is CLAMPED rather than trusted: a negative margin would switch
/// on noise and a fraction above 1.0 could never switch at all, and a config
/// typo must not produce either.
fn margin_paise(spacing_paise: i64, fraction: f64) -> i64 {
    let clamped = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        ATM_SWITCH_MARGIN_FRACTION
    };
    // APPROVED: cast — spacing is a strike gap in paise and the fraction is
    // clamped to [0,1], so the product is bounded by the spacing itself.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "bounded by spacing; a strike gap is far inside f64's exact-integer range"
    )]
    {
        ((spacing_paise as f64) * clamped).round() as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(strike_rupees: f64, ce: i64, pe: i64) -> StrikePair {
        StrikePair {
            strike_paise: (strike_rupees * 100.0).round() as i64,
            ce_security_id: ce,
            pe_security_id: pe,
        }
    }

    fn minute<'a>(u: &'a str, spot: f64, pairs: &'a [StrikePair]) -> ChainMinute<'a> {
        ChainMinute {
            underlying: u,
            spot,
            pairs,
        }
    }

    fn nifty_chain() -> Vec<StrikePair> {
        // 50-point spacing, the real NIFTY shape.
        vec![
            pair(24_200.0, 101, 201),
            pair(24_250.0, 102, 202),
            pair(24_300.0, 103, 203),
            pair(24_350.0, 104, 204),
            pair(24_400.0, 105, 205),
        ]
    }

    // ── adoption ────────────────────────────────────────────────────────────

    #[test]
    fn the_first_minute_adopts_immediately_without_hysteresis() {
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let c = nifty_chain();
        let sw = t
            .observe(&minute("NIFTY", 24_305.0, &c))
            .expect("first minute must adopt");
        assert_eq!(sw.reason, SwitchReason::FirstAdoption);
        assert_eq!(sw.from, None);
        assert_eq!(sw.to.strike_paise, 2_430_000);
        assert_eq!(sw.underlying_index, 0);
    }

    #[test]
    fn an_unchanged_atm_costs_no_socket_action() {
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let c = nifty_chain();
        t.observe(&minute("NIFTY", 24_305.0, &c)).unwrap();
        for _ in 0..50 {
            assert_eq!(
                t.observe(&minute("NIFTY", 24_306.0, &c)),
                Err(NoSwitch::AlreadyAtm),
                "a quiet minute must not touch the socket"
            );
        }
    }

    // ── hysteresis ──────────────────────────────────────────────────────────

    #[test]
    fn a_move_inside_the_deadband_never_switches() {
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let c = nifty_chain();
        t.observe(&minute("NIFTY", 24_300.0, &c)).unwrap();
        // 24,313 is nearer 24,300 than 24,350; well inside the band.
        for _ in 0..20 {
            assert_eq!(
                t.observe(&minute("NIFTY", 24_313.0, &c)),
                Err(NoSwitch::AlreadyAtm)
            );
        }
        assert_eq!(t.current(0).unwrap().strike_paise, 2_430_000);
    }

    #[test]
    fn a_challenger_must_be_confirmed_before_it_takes_the_socket() {
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let c = nifty_chain();
        t.observe(&minute("NIFTY", 24_300.0, &c)).unwrap();
        // Decisively nearer 24,350.
        assert_eq!(
            t.observe(&minute("NIFTY", 24_349.0, &c)),
            Err(NoSwitch::AwaitingConfirmation),
            "one observation is not enough to move a live subscription"
        );
        let sw = t
            .observe(&minute("NIFTY", 24_349.0, &c))
            .expect("second confirming observation must switch");
        assert_eq!(sw.reason, SwitchReason::SpotMoved);
        assert_eq!(sw.to.strike_paise, 2_435_000);
    }

    #[test]
    fn a_single_anomalous_print_cannot_move_the_socket() {
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let c = nifty_chain();
        t.observe(&minute("NIFTY", 24_300.0, &c)).unwrap();
        assert_eq!(
            t.observe(&minute("NIFTY", 24_398.0, &c)),
            Err(NoSwitch::AwaitingConfirmation)
        );
        // The spike retreats; the challenger must be forgotten, not banked.
        assert_eq!(
            t.observe(&minute("NIFTY", 24_301.0, &c)),
            Err(NoSwitch::AlreadyAtm)
        );
        assert_eq!(
            t.observe(&minute("NIFTY", 24_398.0, &c)),
            Err(NoSwitch::AwaitingConfirmation),
            "the count must restart, not resume"
        );
    }

    #[test]
    fn the_midpoint_cannot_flap_the_subscription() {
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let c = nifty_chain();
        t.observe(&minute("NIFTY", 24_300.0, &c)).unwrap();
        // Oscillate either side of the 24,325 midpoint for an hour.
        for i in 0..60 {
            let spot = if i % 2 == 0 { 24_324.0 } else { 24_326.0 };
            let r = t.observe(&minute("NIFTY", spot, &c));
            assert!(
                r.is_err(),
                "midpoint chatter must never switch, minute {i} gave {r:?}"
            );
        }
        assert_eq!(t.current(0).unwrap().strike_paise, 2_430_000);
    }

    // ── the two subtle correctness traps ────────────────────────────────────

    #[test]
    fn the_same_strike_with_a_new_contract_id_still_reswitches() {
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let c = nifty_chain();
        t.observe(&minute("NIFTY", 24_300.0, &c)).unwrap();
        // Same strike, ids rolled — Dhan documents derivative ids as unstable
        // across days. The SUBSCRIPTION KEY is the id, not the price.
        let rolled = vec![
            pair(24_200.0, 901, 951),
            pair(24_250.0, 902, 952),
            pair(24_300.0, 903, 953),
            pair(24_350.0, 904, 954),
        ];
        let sw = t
            .observe(&minute("NIFTY", 24_300.0, &rolled))
            .expect("a new id at the same strike must re-subscribe");
        assert_eq!(sw.reason, SwitchReason::ContractIdChanged);
        assert_eq!(sw.to.ce_security_id, 903);
    }

    #[test]
    fn a_vanished_subscribed_strike_switches_immediately() {
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let c = nifty_chain();
        t.observe(&minute("NIFTY", 24_300.0, &c)).unwrap();
        // Expiry roll: the chain no longer lists 24,300 at all.
        let rolled = vec![pair(24_500.0, 301, 401), pair(24_550.0, 302, 402)];
        let sw = t
            .observe(&minute("NIFTY", 24_505.0, &rolled))
            .expect("holding a subscription to a delisted contract is worse than switching");
        assert_eq!(sw.reason, SwitchReason::SubscribedStrikeVanished);
        assert_eq!(sw.to.strike_paise, 2_450_000);
    }

    // ── refusals: a bad input never costs a live subscription ───────────────

    #[test]
    fn every_unusable_input_keeps_the_current_subscription() {
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let c = nifty_chain();
        t.observe(&minute("NIFTY", 24_300.0, &c)).unwrap();
        let held = t.current(0).unwrap();

        for (spot, want) in [
            (f64::NAN, NoSwitch::UnusableSpot),
            (f64::INFINITY, NoSwitch::UnusableSpot),
            (0.0, NoSwitch::UnusableSpot),
            (-1.0, NoSwitch::UnusableSpot),
        ] {
            assert_eq!(t.observe(&minute("NIFTY", spot, &c)), Err(want));
            assert_eq!(t.current(0).unwrap(), held, "must keep what it had");
        }

        assert_eq!(
            t.observe(&minute("NIFTY", 24_300.0, &[])),
            Err(NoSwitch::NoUsablePairs)
        );
        assert_eq!(t.current(0).unwrap(), held);
    }

    #[test]
    fn a_zero_contract_id_is_never_subscribed() {
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        // 0 is the documented ABSENT sentinel; subscribing it asks the socket
        // for instrument 0 and looks perfectly healthy while carrying nothing.
        let bad = vec![pair(24_300.0, 0, 203), pair(24_350.0, 104, 0)];
        assert_eq!(
            t.observe(&minute("NIFTY", 24_305.0, &bad)),
            Err(NoSwitch::NoUsablePairs)
        );
        assert_eq!(t.tracked_len(), 0, "nothing may be adopted from bad rows");
    }

    #[test]
    fn a_pair_whose_legs_share_an_id_is_refused() {
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let corrupt = vec![pair(24_300.0, 777, 777)];
        assert_eq!(
            t.observe(&minute("NIFTY", 24_300.0, &corrupt)),
            Err(NoSwitch::NoUsablePairs),
            "one socket would shadow the other and the pair would be half a pair"
        );
    }

    #[test]
    fn an_untracked_underlying_is_refused_not_adopted() {
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let c = nifty_chain();
        assert_eq!(
            t.observe(&minute("FINNIFTY", 24_300.0, &c)),
            Err(NoSwitch::UntrackedUnderlying)
        );
        assert_eq!(t.tracked_len(), 0);
    }

    // ── determinism, bounds, shape ──────────────────────────────────────────

    #[test]
    fn an_exact_midpoint_resolves_deterministically() {
        let c = nifty_chain();
        // 24,325 is exactly between 24,300 and 24,350.
        let mut a = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let mut b = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let ra = a.observe(&minute("NIFTY", 24_325.0, &c)).unwrap();
        let rb = b.observe(&minute("NIFTY", 24_325.0, &c)).unwrap();
        assert_eq!(ra.to, rb.to, "equidistant strikes must resolve identically");
        assert_eq!(
            ra.to.strike_paise, 2_430_000,
            "ties break to the LOWER strike"
        );
    }

    #[test]
    fn the_tracker_map_is_bounded_by_the_configured_underlyings() {
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let c = nifty_chain();
        for _ in 0..10_000 {
            let _ = t.observe(&minute("NIFTY", 24_300.0, &c));
            let _ = t.observe(&minute("BANKNIFTY", 24_300.0, &c));
            let _ = t.observe(&minute("FINNIFTY", 24_300.0, &c));
            let _ = t.observe(&minute("GARBAGE", 24_300.0, &c));
        }
        assert!(
            t.tracked_len() <= DEPTH_200_ATM_UNDERLYINGS.len(),
            "vendor input must never grow the map, got {}",
            t.tracked_len()
        );
    }

    #[test]
    fn the_two_underlyings_are_tracked_independently() {
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let c = nifty_chain();
        t.observe(&minute("NIFTY", 24_300.0, &c)).unwrap();
        t.observe(&minute("BANKNIFTY", 24_400.0, &c)).unwrap();
        assert_eq!(t.current(0).unwrap().strike_paise, 2_430_000);
        assert_eq!(t.current(1).unwrap().strike_paise, 2_440_000);
        // Moving one must not disturb the other.
        let _ = t.observe(&minute("NIFTY", 24_399.0, &c));
        let _ = t.observe(&minute("NIFTY", 24_399.0, &c));
        assert_eq!(t.current(1).unwrap().strike_paise, 2_440_000);
    }

    #[test]
    fn the_socket_budget_is_four_and_matches_the_underlying_list() {
        assert_eq!(DEPTH_200_ATM_SOCKETS, 4);
        assert_eq!(DEPTH_200_ATM_UNDERLYINGS.len() * 2, DEPTH_200_ATM_SOCKETS);
        assert_eq!(tracked_underlying_index("nifty"), Some(0));
        assert_eq!(tracked_underlying_index(" BANKNIFTY "), Some(1));
        assert_eq!(tracked_underlying_index("SENSEX"), None);
    }

    // ── config robustness ───────────────────────────────────────────────────

    #[test]
    fn an_absurd_margin_config_cannot_break_the_tracker() {
        for fraction in [-5.0_f64, 0.0, 1.0, 99.0, f64::NAN, f64::INFINITY] {
            let mut t = Depth200AtmTracker::new(Depth200AtmConfig {
                switch_margin_fraction: fraction,
                confirm_observations: 0, // clamped to 1
            });
            let c = nifty_chain();
            t.observe(&minute("NIFTY", 24_300.0, &c)).unwrap();
            // Whatever the config, a far move must eventually resolve and the
            // tracker must never panic or lose its current pair.
            for _ in 0..5 {
                let _ = t.observe(&minute("NIFTY", 24_400.0, &c));
            }
            assert!(t.current(0).is_some(), "fraction {fraction} lost the pair");
        }
    }

    #[test]
    fn an_irregular_chain_still_yields_a_usable_spacing() {
        // A missing strike doubles one gap; the MEDIAN must not be dragged.
        let gappy = vec![
            pair(24_200.0, 101, 201),
            pair(24_250.0, 102, 202),
            // 24,300 absent
            pair(24_350.0, 104, 204),
            pair(24_400.0, 105, 205),
        ];
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        t.observe(&minute("NIFTY", 24_250.0, &gappy)).unwrap();
        assert_eq!(t.current(0).unwrap().strike_paise, 2_425_000);
        // A decisive move to 24,350 must still be able to switch.
        let _ = t.observe(&minute("NIFTY", 24_349.0, &gappy));
        let r = t.observe(&minute("NIFTY", 24_349.0, &gappy));
        assert!(
            r.is_ok(),
            "an irregular chain must not freeze the tracker: {r:?}"
        );
    }

    #[test]
    fn a_single_strike_chain_keeps_what_it_has_rather_than_guessing() {
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let c = nifty_chain();
        t.observe(&minute("NIFTY", 24_300.0, &c)).unwrap();
        // Chain collapses to the one strike we already hold: no spacing can be
        // inferred, so no deadband can be applied. Keep the subscription.
        let lone = vec![pair(24_300.0, 103, 203)];
        assert_eq!(
            t.observe(&minute("NIFTY", 24_390.0, &lone)),
            Err(NoSwitch::AlreadyAtm)
        );
        assert_eq!(t.current(0).unwrap().strike_paise, 2_430_000);
    }

    #[test]
    fn an_enormous_chain_is_handled_without_panic() {
        let big: Vec<StrikePair> = (0..10_000)
            .map(|i| {
                pair(
                    10_000.0 + f64::from(i) * 50.0,
                    1_000 + i64::from(i),
                    500_000 + i64::from(i),
                )
            })
            .collect();
        let mut t = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let sw = t.observe(&minute("NIFTY", 260_000.0, &big)).unwrap();
        assert_eq!(sw.reason, SwitchReason::FirstAdoption);
    }
}
