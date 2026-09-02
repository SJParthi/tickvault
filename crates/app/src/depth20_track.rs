//! Per-minute tracking for the five depth-20 sockets.
//!
//! # What this closes
//!
//! The attach chooses the operator's depth-20 layout once
//! ([`crate::depth20_layout`]) and then never revisits it. Depth-200 has
//! tracked at-the-money since it was built; depth-20 has not, so its 250
//! instruments were whatever 09:1x looked like, for the rest of the day.
//!
//! Both halves decay, and they decay differently:
//!
//! - The **index windows** are ±12 strikes around spot. NIFTY moving 150
//!   points walks three strikes off one end of the window, so the socket is
//!   holding depth for contracts the money has left while the strikes that
//!   now matter are unsubscribed.
//! - The **movers sockets** are today's 37 biggest gainers and losers. That
//!   ranking is a morning ranking by 09:16; the real movers of the day are
//!   frequently not in it.
//!
//! # Why swaps, and why they are PAIRED
//!
//! A depth-20 connection holds up to 50 instruments, so this is not the
//! depth-200 shape of "one socket, one contract, replace it". The desired set
//! is recomputed whole and diffed against the wire.
//!
//! Each socket's diff is then PAIRED — one departure released for each
//! arrival taken — and the surplus on either side is counted and dropped
//! rather than sent. That is not a simplification; it is what keeps the
//! connection inside its own capacity without this module having to track it:
//!
//! - An unpaired ARRIVAL would push the socket past 50. Dhan does not
//!   silently ignore the excess.
//! - An unpaired DEPARTURE would leave the socket carrying fewer than 50 for
//!   no gain — the strike it would free has no arrival waiting for it.
//!
//! In the healthy minute the two sides are equal by construction (a window
//! that shifts by one strike releases exactly the two legs it takes), so the
//! surplus path is the degraded case, and it degrades by holding position.
//!
//! # Ordering within a socket
//!
//! Departures and arrivals are each sorted by `(security_id, segment)` before
//! pairing. Two records of the same minute must produce the same wire
//! traffic, or a swap that the guard refuses once is refused forever — the
//! failure mode `plan_minute` already documents for depth-200.

use std::collections::BTreeSet;

use tickvault_core::websocket::pool_supervisor::{
    LiveSubscriptionCommand, SubscribeInstrument, SwapOutcome,
};

use crate::depth20_layout::Depth20Layout;

/// A single instrument's identity, ordered so a diff is deterministic.
///
/// `SubscribeInstrument` is not `Ord`, and sorting is what makes two runs of
/// the same minute produce the same wire traffic.
type Key = (u64, u8);

#[must_use]
fn key_of(instrument: SubscribeInstrument) -> Key {
    (instrument.security_id, instrument.segment as u8)
}

/// One socket's changes for this minute.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Depth20SocketPlan {
    /// Index into the depth-20 pool, `0..5`.
    pub socket: usize,
    /// `(release, take)` pairs, in a deterministic order.
    pub swaps: Vec<(SubscribeInstrument, SubscribeInstrument)>,
    /// Arrivals with no departure to fund them — counted, never sent.
    pub unfunded_arrivals: usize,
    /// Departures with no arrival to use the slot — counted, never sent.
    pub unused_departures: usize,
}

/// Every socket's changes for this minute.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Depth20Plan {
    /// Only sockets with at least one swap. A quiet socket is absent.
    pub sockets: Vec<Depth20SocketPlan>,
    /// Sockets whose desired set came back empty and were therefore left
    /// alone. See [`plan_depth20_minute`].
    pub sockets_left_alone: usize,
}

impl Depth20Plan {
    /// Whether nothing needs to go on the wire.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.sockets.is_empty()
    }

    /// Total swaps across every socket.
    #[must_use]
    pub fn swap_count(&self) -> usize {
        self.sockets.iter().map(|s| s.swaps.len()).sum()
    }
}

/// Pairs each wire socket with the layout socket it actually corresponds to.
///
/// # Why this is not just the index
///
/// It is tempting to assume wire socket N holds layout socket N, and at a full
/// 250-instrument layout that happens to be true. It is NOT true in general,
/// and the case where it breaks is ordinary rather than exotic.
///
/// `plan_pool` SPREADS a depth set: it shards the flattened list into
/// `len.div_ceil(connections)` chunks. At 250 that is five chunks of 50, which
/// lines up with the layout's five sockets exactly. At 230 — one index window
/// short near a freshly-listed expiry, or fewer than 37 movers resolving,
/// both routine at 09:13 — it is five chunks of 46, and every boundary after
/// the first has moved.
///
/// Wire socket 1 then holds the tail of NIFTY's window plus the head of
/// BANKNIFTY's. Diffing it against layout socket 1 finds almost nothing in
/// common, so the planner would swap nearly the whole socket, every minute,
/// for the rest of the session — burning the connection's entire capacity on
/// churn while reporting healthy swap counts the whole time.
///
/// Matching on CONTENT removes the assumption instead of documenting it, and
/// keeps working if `plan_pool`'s sharding ever changes again.
///
/// # Two passes, because neither signal is sufficient alone
///
/// **Position, where the SHAPE agrees.** Wire socket i holding exactly as
/// many instruments as layout socket i is not a coincidence — it is the
/// arrangement the dial produced. This pass exists because overlap ALONE
/// would refuse the one case that matters most on a movers socket: a ranking
/// that turned over completely shares nothing with what the socket holds, and
/// must still be re-aimed rather than frozen for the session.
///
/// **Content, for the rest.** A size mismatch means the shard boundaries
/// moved, so position means nothing and only overlap can say what a socket
/// is.
///
/// Greedy by best overlap, each layout socket claimed at most once: two wire
/// sockets that both partly overlap one layout socket must not both diff
/// against it, or the loser would be told to take instruments the winner is
/// already taking. A wire socket sharing NOTHING with any unclaimed layout
/// socket is left unpaired — refusing to guess is what makes a genuinely
/// unrecognisable socket hold position rather than churn.
///
/// # Complexity
///
/// O(sockets^2 x instruments) — 25 comparisons of at most 50 items, once a
/// minute. The set sizes here are fixed by the endpoint's own budget.
#[must_use]
fn match_sockets_by_overlap(
    held: &[Vec<SubscribeInstrument>],
    desired: &Depth20Layout,
) -> Vec<Option<usize>> {
    let want_keys: Vec<BTreeSet<Key>> = desired
        .sockets
        .iter()
        .map(|s| s.instruments.iter().copied().map(key_of).collect())
        .collect();
    let mut claimed = vec![false; want_keys.len()];
    let mut out = vec![None; held.len()];

    // PASS 1 — the position, where the SHAPE agrees.
    //
    // `plan_pool` shards the flattened layout in order, so when wire socket i
    // holds exactly as many instruments as layout socket i, position is not a
    // guess: it is the arrangement the dial produced. Taking it first is what
    // preserves the legitimate case overlap alone cannot express — a movers
    // socket whose ENTIRE ranking turned over shares nothing with what it
    // holds, and must still be re-aimed rather than frozen.
    for (index, held_here) in held.iter().enumerate() {
        // DISTINCT count on both sides. Comparing the layout's deduplicated
        // key count against the wire's RAW item count made a socket carrying
        // one repeated instrument look like a different shape entirely, so
        // position was discarded and the socket fell through to an overlap
        // pass that could not identify it either — leaving it frozen. Found
        // by the duplicate-subscribe test, which failed for this reason
        // rather than the one it was written for.
        let have_distinct = held_here
            .iter()
            .copied()
            .map(key_of)
            .collect::<BTreeSet<Key>>()
            .len();
        if want_keys
            .get(index)
            .is_some_and(|k| k.len() == have_distinct)
            && !claimed[index]
        {
            claimed[index] = true;
            out[index] = Some(index);
        }
    }

    // PASS 2 — content, for whatever the shape could not vouch for.
    //
    // A size mismatch at position i means the shard boundaries moved, so the
    // position means nothing and only overlap can say what this socket is.
    for (index, held_here) in held.iter().enumerate() {
        if out[index].is_some() {
            continue;
        }
        let have: BTreeSet<Key> = held_here.iter().copied().map(key_of).collect();
        let mut best: Option<(usize, usize)> = None;
        for (w, keys) in want_keys.iter().enumerate() {
            if claimed[w] {
                continue;
            }
            let overlap = keys.intersection(&have).count();
            if overlap > 0 && best.is_none_or(|(_, b)| overlap > b) {
                best = Some((w, overlap));
            }
        }
        if let Some((w, _)) = best {
            claimed[w] = true;
            out[index] = Some(w);
        }
    }
    out
}

/// Diffs the desired layout against what the wire holds, per socket.
///
/// # What is deliberately NOT done
///
/// **A socket whose desired set is empty is left completely alone**, and
/// counted in `sockets_left_alone`. An empty desired set means the chain did
/// not publish this minute, or the ranking query returned nothing — not that
/// the operator wants no depth. Acting on it would strip a working socket on
/// the strength of a missing input, which is the same trade the attach
/// already refuses when it keeps the adaptive selection over an empty layout.
///
/// # Complexity
///
/// O(n log n) in one socket's instruments to order the diff, over 50 — a cold
/// path that runs once a minute.
#[must_use]
pub fn plan_depth20_minute(
    held: &[Vec<SubscribeInstrument>],
    desired: &Depth20Layout,
) -> Depth20Plan {
    let mut plan = Depth20Plan::default();

    // Which layout socket each wire socket actually IS, by content — never by
    // position. See `match_sockets_by_overlap`.
    let pairing = match_sockets_by_overlap(held, desired);

    // Instruments already claimed by an EARLIER socket in this same plan.
    //
    // Per-socket deduplication is not enough. Two layout sockets can list
    // the same instrument — nothing forbids it, and a movers ranking that
    // named a stock twice or a chain that repeated a strike would produce
    // exactly that — and each is then paired to a different connection.
    // Both would take it in the same minute: two of the 250 authorized
    // slots spent on one order book, and on Dhan's per-connection
    // subscription state the second is a duplicate.
    //
    // First socket in wire order wins, which is deterministic. The loser
    // simply has one fewer arrival, so the departure it would have funded
    // stays put — a slot held rather than wasted.
    //
    // Found by a property test, not by reading: the hand-written suite had
    // a cross-socket case and it happened not to collide.
    let mut claimed_takes: BTreeSet<Key> = BTreeSet::new();

    for (index, held_here) in held.iter().enumerate() {
        let Some(want) = pairing[index].and_then(|w| desired.sockets.get(w)) else {
            // No layout socket recognisably corresponds to this connection.
            // Nothing to compare against, so nothing moves.
            plan.sockets_left_alone += 1;
            continue;
        };
        if want.instruments.is_empty() {
            plan.sockets_left_alone += 1;
            continue;
        }

        let have: BTreeSet<Key> = held_here.iter().copied().map(key_of).collect();
        let want_keys: BTreeSet<Key> = want.instruments.iter().copied().map(key_of).collect();

        // Sorted by construction — BTreeSet iterates in key order — so the
        // pairing below is deterministic without an explicit sort.
        let mut departures: Vec<SubscribeInstrument> = held_here
            .iter()
            .copied()
            .filter(|i| !want_keys.contains(&key_of(*i)))
            .collect();
        let mut arrivals: Vec<SubscribeInstrument> = want
            .instruments
            .iter()
            .copied()
            .filter(|i| !have.contains(&key_of(*i)))
            .collect();
        departures.sort_unstable_by_key(|i| key_of(*i));
        arrivals.sort_unstable_by_key(|i| key_of(*i));
        // DEDUP, and this is not defensive tidiness — it is the difference
        // between a working connection and a dead one.
        //
        // Neither list is guaranteed unique. `arrivals` is filtered from the
        // layout's own Vec, so an underlying appearing twice in one socket —
        // a stock ranking as both a gainer and a loser, a chain listing a
        // strike twice — yields the same instrument twice. Both copies pass
        // the "not already held" filter, and the planner would then pair each
        // with a different departure and subscribe it TWICE. Dhan answers a
        // duplicate subscribe with an 804, which is Fatal: the connection
        // drops and does not come back this session.
        //
        // `departures` is deduped for the mirror reason — unsubscribing the
        // same instrument twice frees one slot while spending two, so the
        // socket silently ends the minute below its dialed size.
        //
        // Caught by attacking this module's own output rather than by
        // reasoning about it; the first version shipped with both lists
        // undeduped and eleven tests passing.
        departures.dedup_by_key(|i| key_of(*i));
        arrivals.dedup_by_key(|i| key_of(*i));
        // Plan-wide, not just socket-wide. See `claimed_takes`.
        arrivals.retain(|i| !claimed_takes.contains(&key_of(*i)));

        if departures.is_empty() && arrivals.is_empty() {
            continue;
        }

        let paired = departures.len().min(arrivals.len());
        let socket_plan = Depth20SocketPlan {
            socket: index,
            swaps: departures
                .iter()
                .copied()
                .zip(arrivals.iter().copied())
                .take(paired)
                .collect(),
            unfunded_arrivals: arrivals.len() - paired,
            unused_departures: departures.len() - paired,
        };
        for (_, take) in &socket_plan.swaps {
            claimed_takes.insert(key_of(*take));
        }
        if !socket_plan.swaps.is_empty()
            || socket_plan.unfunded_arrivals > 0
            || socket_plan.unused_departures > 0
        {
            plan.sockets.push(socket_plan);
        }
    }

    plan
}

// ---------------------------------------------------------------------------
// Applying a plan
// ---------------------------------------------------------------------------

/// One live depth-20 connection (the wire side; the layout's `Depth20Socket` is the desired side).
///
/// Unlike a depth-200 socket, which holds exactly one contract, this holds up
/// to [`crate::depth20_layout::DEPTH_20_PER_SOCKET`] — so `held` is a set, and
/// keeping it in step with the wire is what makes the next minute's diff
/// correct.
#[derive(Debug)]
pub struct Depth20LiveSocket {
    /// Where a swap travels.
    pub tx: tokio::sync::mpsc::Sender<LiveSubscriptionCommand>,
    /// What this connection is believed to hold.
    pub held: Vec<SubscribeInstrument>,
    /// Swaps sent this minute and not yet answered by the connection task.
    ///
    /// `held` moves the moment a swap is QUEUED; the connection answers
    /// through these once the guard and the wire have both had their say,
    /// and [`reconcile_pending_depth20_swaps`] folds each answer back at
    /// the top of the next minute. Bounded by the swaps one socket can
    /// take per minute (≤ its window, 50), read once a minute.
    pub pending: Vec<PendingDepth20SwapAck>,
}

/// One depth-20 swap the connection task has been handed but not answered.
///
/// Carries both halves so a refused command — or a task that died with it
/// queued — can be reverted exactly: `take` goes back to `release`.
#[derive(Debug)]
pub struct PendingDepth20SwapAck {
    /// The connection's verdict, once it has one.
    ack: tokio::sync::oneshot::Receiver<SwapOutcome>,
    /// The instrument the swap released.
    release: SubscribeInstrument,
    /// The instrument the swap took.
    take: SubscribeInstrument,
}

/// The counter for depth-20 swaps that reached a channel.
pub const DEPTH20_SWAPS_SENT: &str = "tv_depth20_track_swaps_sent_total";

/// The counter for depth-20 swaps that did not, by reason.
pub const DEPTH20_SWAPS_REFUSED: &str = "tv_depth20_track_swaps_refused_total";

/// The counter for diffs that could not be paired, by side.
pub const DEPTH20_UNPAIRED: &str = "tv_depth20_track_unpaired_total";

/// Registers every counter at zero.
///
/// An alarm on a counter that has never been emitted reads as missing data
/// rather than as zero, so a quiet session and a dead exporter look identical
/// until the first swap.
pub fn pre_register_depth20_counters() {
    metrics::counter!(DEPTH20_SWAPS_SENT).increment(0);
    for reason in ["channel_full", "channel_closed", "no_socket", "not_held"] {
        metrics::counter!(DEPTH20_SWAPS_REFUSED, "reason" => reason).increment(0);
    }
    for side in ["arrival", "departure"] {
        metrics::counter!(DEPTH20_UNPAIRED, "side" => side).increment(0);
    }
}

/// Sends a plan and returns how many swaps reached a channel.
///
/// # Why `held` moves only on a successful send
///
/// The same reason `depth_rebalance::send_swap` does it: a believed-held set
/// that ran ahead of the wire would make the NEXT minute's diff name an
/// instrument the connection never received, and the subscribe guard refuses
/// that — one dropped message costing every future swap on that socket.
pub fn apply_depth20_plan(sockets: &mut [Depth20LiveSocket], plan: &Depth20Plan) -> usize {
    let mut sent = 0usize;
    for socket_plan in &plan.sockets {
        if socket_plan.unfunded_arrivals > 0 {
            metrics::counter!(DEPTH20_UNPAIRED, "side" => "arrival")
                .increment(socket_plan.unfunded_arrivals as u64);
        }
        if socket_plan.unused_departures > 0 {
            metrics::counter!(DEPTH20_UNPAIRED, "side" => "departure")
                .increment(socket_plan.unused_departures as u64);
        }
        let Some(socket) = sockets.get_mut(socket_plan.socket) else {
            metrics::counter!(DEPTH20_SWAPS_REFUSED, "reason" => "no_socket")
                .increment(socket_plan.swaps.len() as u64);
            tracing::error!(
                code =
                    tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching.code_str(),
                socket = socket_plan.socket,
                sockets = sockets.len(),
                "depth-20 tracking decided swaps for a socket that does not exist"
            );
            continue;
        };
        for (release, take) in &socket_plan.swaps {
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            let command = LiveSubscriptionCommand::Swap {
                old: *release,
                new: *take,
                ack: Some(ack_tx),
            };
            match socket.tx.try_send(command) {
                Ok(()) => {
                    // Queued, not yet on the wire: the connection's answer
                    // is folded in by `reconcile_pending_depth20_swaps`
                    // next minute, and only THAT settles `held`.
                    socket.pending.push(PendingDepth20SwapAck {
                        ack: ack_rx,
                        release: *release,
                        take: *take,
                    });
                    // Replace in place so the set keeps its dial order; an
                    // order change here would produce a different — though
                    // equivalent — diff next minute, and equivalent is not
                    // the same as identical when a swap is being matched
                    // against what the guard believes.
                    if let Some(slot) = socket.held.iter_mut().find(|h| **h == *release) {
                        *slot = *take;
                    } else {
                        socket.held.push(*take);
                    }
                    // Then purge any FURTHER copy of the released instrument.
                    // The first one is now `take`, so this removes only the
                    // extras.
                    //
                    // One unsubscribe takes the instrument off the wire
                    // outright, so a second believed copy is a belief the
                    // connection does not share — and a believed-held set
                    // running ahead of the wire is exactly what this module's
                    // header says costs every future swap on the socket: next
                    // minute names it as a departure, the guard refuses an
                    // unsubscribe for something the connection does not hold,
                    // and the arrival that swap was funding is lost with it.
                    //
                    // Duplicates cannot reach here from `build_depth20_layout`
                    // today — it dedupes the whole layout, index sockets
                    // included, as of the same change that added this. One
                    // scan of at most fifty items once a minute is the price
                    // of not depending on that staying true.
                    socket.held.retain(|h| *h != *release);
                    metrics::counter!(DEPTH20_SWAPS_SENT).increment(1);
                    sent = sent.saturating_add(1);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    metrics::counter!(DEPTH20_SWAPS_REFUSED, "reason" => "channel_full")
                        .increment(1);
                    tracing::warn!(
                        code = tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching
                            .code_str(),
                        socket = socket_plan.socket,
                        "depth-20 tracking: the command channel is full, so this swap is \
                         dropped rather than awaited. Awaiting would apply backpressure to \
                         the frame drain, and a stalled drain is tick loss. Retried next \
                         minute."
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    metrics::counter!(DEPTH20_SWAPS_REFUSED, "reason" => "channel_closed")
                        .increment(1);
                    tracing::error!(
                        code = tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching
                            .code_str(),
                        socket = socket_plan.socket,
                        "depth-20 tracking: the command channel is CLOSED — this connection \
                         is gone and its window will never move again this session."
                    );
                }
            }
        }
    }
    sent
}
/// Folds every answered depth-20 swap acknowledgement back into `held`.
///
/// Non-blocking (`try_recv`), run at the top of each minute BEFORE `held` is
/// read for the next plan. Returns how many swaps were UN-marked (the taken
/// instrument put back to the released one, in place).
///
/// Same table as `depth_rebalance::reconcile_pending_swaps`: `Held` and the
/// two wire-failure `NotHeld`s keep the advanced belief (the guard already
/// names the new instrument); a REFUSED `NotHeld` and a dropped sender revert
/// it. See [`SwapOutcome::caller_should_unmark`].
// TEST-EXEMPT: pinned by the four depth-20 ack cases (held keeps, refused reverts, dropped sender reverts, emptied or wire_failed keeps)
pub fn reconcile_pending_depth20_swaps(sockets: &mut [Depth20LiveSocket]) -> usize {
    let mut unmarked_total = 0usize;
    for (index, socket) in sockets.iter_mut().enumerate() {
        if socket.pending.is_empty() {
            continue;
        }
        let mut unmarked = 0usize;
        let held = &mut socket.held;
        socket.pending.retain_mut(|p| {
            let unmark = match p.ack.try_recv() {
                Ok(outcome) => outcome.caller_should_unmark(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return true,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => true,
            };
            if unmark {
                // The exact inverse of the apply: put the released instrument
                // back where the taken one was written, so dial order — the
                // thing a depth-20 diff is matched on — is preserved.
                // O(1) EXEMPT: n <= 50 per socket, once a minute, cold path.
                if let Some(slot) = held.iter_mut().find(|h| **h == p.take) {
                    *slot = p.release;
                } else {
                    held.push(p.release);
                }
                unmarked = unmarked.saturating_add(1);
            }
            false
        });
        if unmarked > 0 {
            metrics::counter!(DEPTH20_SWAPS_REFUSED, "reason" => "not_held")
                .increment(unmarked as u64);
            tracing::warn!(
                code =
                    tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching.code_str(),
                socket = index,
                unmarked,
                source = "swap_ack_not_held",
                "depth-20 tracking: the connection did NOT take queued swaps (refused, or the \
                 task ended with them unanswered) — the believed window is reverted so next \
                 minute plans them again from what the socket really carries"
            );
        }
        unmarked_total = unmarked_total.saturating_add(unmarked);
    }
    unmarked_total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depth20_layout::Depth20Socket;
    use tickvault_common::types::ExchangeSegment;

    fn ins(id: u64) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment: ExchangeSegment::NseFno,
        }
    }

    fn socket(ids: &[u64]) -> Depth20Socket {
        Depth20Socket {
            underlying: None,
            instruments: ids.iter().copied().map(ins).collect(),
        }
    }

    fn layout(sockets: Vec<Depth20Socket>) -> Depth20Layout {
        Depth20Layout {
            sockets,
            ..Depth20Layout::default()
        }
    }

    #[test]
    fn a_window_that_shifts_one_strike_swaps_both_its_legs() {
        // The everyday minute: spot moves up one strike, so the bottom
        // strike's CE and PE leave and the new top strike's arrive.
        let held = vec![vec![ins(1), ins(2), ins(3), ins(4)]];
        let want = layout(vec![socket(&[3, 4, 5, 6])]);
        let plan = plan_depth20_minute(&held, &want);
        assert_eq!(plan.swap_count(), 2);
        assert_eq!(
            plan.sockets[0].swaps,
            vec![(ins(1), ins(5)), (ins(2), ins(6))]
        );
        assert_eq!(plan.sockets[0].unfunded_arrivals, 0);
        assert_eq!(plan.sockets[0].unused_departures, 0);
    }

    #[test]
    fn an_unchanged_socket_produces_nothing() {
        let held = vec![vec![ins(1), ins(2)]];
        let want = layout(vec![socket(&[1, 2])]);
        assert!(plan_depth20_minute(&held, &want).is_quiet());
    }

    #[test]
    fn the_order_held_arrives_in_does_not_change_the_plan() {
        // Two records of the same minute must produce the same wire traffic.
        let a = vec![vec![ins(4), ins(1), ins(3), ins(2)]];
        let b = vec![vec![ins(1), ins(2), ins(3), ins(4)]];
        let want = layout(vec![socket(&[3, 4, 5, 6])]);
        assert_eq!(
            plan_depth20_minute(&a, &want),
            plan_depth20_minute(&b, &want)
        );
    }

    #[test]
    fn an_empty_desired_socket_is_left_completely_alone() {
        // The chain did not publish. Stripping a working socket over a
        // missing input is strictly worse than holding position.
        let held = vec![vec![ins(1), ins(2)]];
        let want = layout(vec![socket(&[])]);
        let plan = plan_depth20_minute(&held, &want);
        assert!(plan.is_quiet());
        assert_eq!(plan.sockets_left_alone, 1);
    }

    #[test]
    fn a_layout_with_fewer_sockets_leaves_the_rest_alone() {
        let held = vec![vec![ins(1)], vec![ins(2)], vec![ins(3)]];
        let want = layout(vec![socket(&[9])]);
        let plan = plan_depth20_minute(&held, &want);
        assert_eq!(plan.swap_count(), 1);
        assert_eq!(plan.sockets_left_alone, 2);
    }

    #[test]
    fn an_arrival_with_no_departure_to_fund_it_is_counted_not_sent() {
        // Sending it would push the connection past its 50-instrument cap.
        let held = vec![vec![ins(1), ins(2)]];
        let want = layout(vec![socket(&[1, 2, 3])]);
        let plan = plan_depth20_minute(&held, &want);
        assert_eq!(plan.swap_count(), 0);
        assert_eq!(plan.sockets[0].unfunded_arrivals, 1);
    }

    #[test]
    fn a_departure_with_no_arrival_to_use_it_is_counted_not_sent() {
        // Releasing it would leave the socket short for no gain.
        let held = vec![vec![ins(1), ins(2), ins(3)]];
        let want = layout(vec![socket(&[1, 2])]);
        let plan = plan_depth20_minute(&held, &want);
        assert_eq!(plan.swap_count(), 0);
        assert_eq!(plan.sockets[0].unused_departures, 1);
    }

    #[test]
    fn a_swap_never_takes_something_the_socket_already_holds() {
        // Dhan answers a duplicate subscribe with an 804, which is Fatal and
        // drops the connection.
        let held = vec![vec![ins(1), ins(2), ins(3)]];
        let want = layout(vec![socket(&[2, 3, 4])]);
        let plan = plan_depth20_minute(&held, &want);
        for (release, take) in &plan.sockets[0].swaps {
            assert!(
                held[0].contains(release),
                "released {release:?} which the socket does not hold"
            );
            assert!(
                !held[0].contains(take),
                "took {take:?} which the socket already holds — an 804 and a dead connection"
            );
        }
    }

    #[test]
    fn the_same_id_in_two_segments_is_two_instruments() {
        // I-P1-11: security_id alone is not an identity.
        let held = vec![vec![SubscribeInstrument {
            security_id: 7,
            segment: ExchangeSegment::NseFno,
        }]];
        let want = layout(vec![Depth20Socket {
            underlying: None,
            instruments: vec![SubscribeInstrument {
                security_id: 7,
                segment: ExchangeSegment::BseFno,
            }],
        }]);
        let plan = plan_depth20_minute(&held, &want);
        assert_eq!(plan.swap_count(), 1, "a segment change is a real swap");
    }

    #[test]
    fn a_full_fifty_instrument_window_shift_stays_paired() {
        let held_ids: Vec<u64> = (0..50).collect();
        let want_ids: Vec<u64> = (4..54).collect();
        let held = vec![held_ids.iter().copied().map(ins).collect::<Vec<_>>()];
        let want = layout(vec![socket(&want_ids)]);
        let plan = plan_depth20_minute(&held, &want);
        assert_eq!(plan.swap_count(), 4);
        assert_eq!(plan.sockets[0].unfunded_arrivals, 0);
        assert_eq!(plan.sockets[0].unused_departures, 0);
    }

    #[test]
    fn a_socket_that_turns_over_completely_swaps_every_slot() {
        // A movers socket whose whole ranking changed.
        let held = vec![(0..50).map(ins).collect::<Vec<_>>()];
        let want_ids: Vec<u64> = (100..150).collect();
        let want = layout(vec![socket(&want_ids)]);
        let plan = plan_depth20_minute(&held, &want);
        assert_eq!(plan.swap_count(), 50);
    }

    #[test]
    fn every_socket_is_planned_independently() {
        let held = vec![vec![ins(1), ins(2)], vec![ins(10), ins(11)]];
        let want = layout(vec![socket(&[1, 3]), socket(&[10, 11])]);
        let plan = plan_depth20_minute(&held, &want);
        assert_eq!(plan.sockets.len(), 1, "only the changed socket appears");
        assert_eq!(plan.sockets[0].socket, 0);
    }
}

#[cfg(test)]
mod apply_tests {
    use super::*;
    use crate::depth20_layout::Depth20Socket as DesiredSocket;
    use tickvault_common::types::ExchangeSegment;

    fn ins(id: u64) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment: ExchangeSegment::NseFno,
        }
    }

    fn live(
        ids: &[u64],
        cap: usize,
    ) -> (
        Depth20LiveSocket,
        tokio::sync::mpsc::Receiver<LiveSubscriptionCommand>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(cap);
        (
            Depth20LiveSocket {
                tx,
                held: ids.iter().copied().map(ins).collect(),
                pending: Vec::new(),
            },
            rx,
        )
    }

    fn want(ids: &[u64]) -> Depth20Layout {
        Depth20Layout {
            sockets: vec![DesiredSocket {
                underlying: None,
                instruments: ids.iter().copied().map(ins).collect(),
            }],
            ..Depth20Layout::default()
        }
    }

    #[tokio::test]
    async fn a_sent_swap_puts_the_right_command_on_the_channel() {
        let (socket, mut rx) = live(&[1, 2], 4);
        let mut sockets = vec![socket];
        let held = vec![sockets[0].held.clone()];
        let plan = plan_depth20_minute(&held, &want(&[2, 3]));
        assert_eq!(apply_depth20_plan(&mut sockets, &plan), 1);
        match rx.try_recv().expect("a command") {
            LiveSubscriptionCommand::Swap { old, new, .. } => {
                assert_eq!(old, ins(1));
                assert_eq!(new, ins(3));
            }
            other => panic!("expected a swap, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn held_tracks_the_wire_so_the_next_minute_diffs_correctly() {
        let (socket, _rx) = live(&[1, 2], 4);
        let mut sockets = vec![socket];
        let held = vec![sockets[0].held.clone()];
        let plan = plan_depth20_minute(&held, &want(&[2, 3]));
        apply_depth20_plan(&mut sockets, &plan);
        // The next minute wants the same thing — and must now be quiet.
        let held_now = vec![sockets[0].held.clone()];
        assert!(
            plan_depth20_minute(&held_now, &want(&[2, 3])).is_quiet(),
            "held did not follow the wire: {:?}",
            sockets[0].held
        );
    }

    /// Pulls the ack sender out of the one command a socket received, so a
    /// test can play the connection task's part.
    fn take_ack(
        rx: &mut tokio::sync::mpsc::Receiver<LiveSubscriptionCommand>,
    ) -> tokio::sync::oneshot::Sender<SwapOutcome> {
        match rx.try_recv().expect("one command") {
            LiveSubscriptionCommand::Swap { ack: Some(ack), .. } => ack,
            other => panic!("expected a swap carrying an ack, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_held_ack_keeps_the_advanced_window() {
        let (socket, mut rx) = live(&[1, 2], 4);
        let mut sockets = vec![socket];
        let held = vec![sockets[0].held.clone()];
        let plan = plan_depth20_minute(&held, &want(&[2, 3]));
        assert_eq!(apply_depth20_plan(&mut sockets, &plan), 1);
        assert_eq!(sockets[0].pending.len(), 1);
        // Unanswered: still pending, nothing reverted.
        assert_eq!(reconcile_pending_depth20_swaps(&mut sockets), 0);
        assert_eq!(sockets[0].pending.len(), 1);
        take_ack(&mut rx)
            .send(SwapOutcome::Held)
            .expect("receiver alive");
        assert_eq!(reconcile_pending_depth20_swaps(&mut sockets), 0);
        assert!(sockets[0].pending.is_empty());
        assert_eq!(sockets[0].held, vec![ins(3), ins(2)]);
    }

    #[tokio::test]
    async fn a_refused_ack_puts_the_released_instrument_back_in_place() {
        // Audit finding 8: `held` ran ahead of the wire on a queued command,
        // and a refused swap left the window naming an instrument the
        // connection never received — so every later diff was wrong.
        let (socket, mut rx) = live(&[1, 2], 4);
        let mut sockets = vec![socket];
        let held = vec![sockets[0].held.clone()];
        let plan = plan_depth20_minute(&held, &want(&[2, 3]));
        apply_depth20_plan(&mut sockets, &plan);
        take_ack(&mut rx)
            .send(SwapOutcome::NotHeld {
                reason: SwapOutcome::REASON_REFUSED,
            })
            .expect("receiver alive");
        assert_eq!(reconcile_pending_depth20_swaps(&mut sockets), 1);
        assert_eq!(
            sockets[0].held,
            vec![ins(1), ins(2)],
            "the released instrument must go back where the taken one was"
        );
        assert!(sockets[0].pending.is_empty());
        // And next minute plans the same swap again, from the truth.
        let held_now = vec![sockets[0].held.clone()];
        assert!(!plan_depth20_minute(&held_now, &want(&[2, 3])).is_quiet());
    }

    #[tokio::test]
    async fn a_dropped_ack_sender_reverts_the_window() {
        let (socket, rx) = live(&[1, 2], 4);
        let mut sockets = vec![socket];
        let held = vec![sockets[0].held.clone()];
        let plan = plan_depth20_minute(&held, &want(&[2, 3]));
        apply_depth20_plan(&mut sockets, &plan);
        drop(rx);
        assert_eq!(reconcile_pending_depth20_swaps(&mut sockets), 1);
        assert_eq!(sockets[0].held, vec![ins(1), ins(2)]);
    }

    #[tokio::test]
    async fn an_emptied_ack_keeps_the_window_the_guard_already_names() {
        // The guard was replaced in place before the wire moved and a
        // redial replays it; reverting would diff against a guard that no
        // longer holds the released instrument, refused every minute.
        let (socket, mut rx) = live(&[1, 2], 4);
        let mut sockets = vec![socket];
        let held = vec![sockets[0].held.clone()];
        let plan = plan_depth20_minute(&held, &want(&[2, 3]));
        apply_depth20_plan(&mut sockets, &plan);
        take_ack(&mut rx)
            .send(SwapOutcome::NotHeld {
                reason: SwapOutcome::REASON_EMPTIED,
            })
            .expect("receiver alive");
        assert_eq!(reconcile_pending_depth20_swaps(&mut sockets), 0);
        assert_eq!(sockets[0].held, vec![ins(3), ins(2)]);
        assert!(sockets[0].pending.is_empty());
    }

    #[tokio::test]
    async fn held_does_not_move_when_the_channel_is_full() {
        // The failure this guards: a believed-held set running ahead of the
        // wire makes every future swap on this socket name an instrument the
        // connection never received.
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        // Fill it.
        tx.try_send(LiveSubscriptionCommand::Extend {
            more: vec![],
            ack: None,
        })
        .expect("prime");
        let mut sockets = vec![Depth20LiveSocket {
            tx,
            held: vec![ins(1), ins(2)],
            pending: Vec::new(),
        }];
        let held = vec![sockets[0].held.clone()];
        let plan = plan_depth20_minute(&held, &want(&[2, 3]));
        assert_eq!(apply_depth20_plan(&mut sockets, &plan), 0);
        assert_eq!(
            sockets[0].held,
            vec![ins(1), ins(2)],
            "held moved on a send that never happened"
        );
    }

    #[tokio::test]
    async fn a_closed_channel_is_counted_and_never_panics() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        drop(rx);
        let mut sockets = vec![Depth20LiveSocket {
            tx,
            held: vec![ins(1), ins(2)],
            pending: Vec::new(),
        }];
        let held = vec![sockets[0].held.clone()];
        let plan = plan_depth20_minute(&held, &want(&[2, 3]));
        assert_eq!(apply_depth20_plan(&mut sockets, &plan), 0);
        assert_eq!(sockets[0].held, vec![ins(1), ins(2)]);
    }

    #[tokio::test]
    async fn a_plan_for_a_socket_that_does_not_exist_is_counted_not_panicked() {
        let mut sockets: Vec<Depth20LiveSocket> = Vec::new();
        let plan = Depth20Plan {
            sockets: vec![Depth20SocketPlan {
                socket: 3,
                swaps: vec![(ins(1), ins(2))],
                unfunded_arrivals: 0,
                unused_departures: 0,
            }],
            sockets_left_alone: 0,
        };
        assert_eq!(apply_depth20_plan(&mut sockets, &plan), 0);
    }

    #[tokio::test]
    async fn a_quiet_plan_sends_nothing() {
        let (socket, mut rx) = live(&[1, 2], 4);
        let mut sockets = vec![socket];
        let held = vec![sockets[0].held.clone()];
        let plan = plan_depth20_minute(&held, &want(&[1, 2]));
        assert_eq!(apply_depth20_plan(&mut sockets, &plan), 0);
        assert!(
            rx.try_recv().is_err(),
            "a quiet minute put a command on the wire"
        );
    }

    #[tokio::test]
    async fn the_socket_never_grows_past_what_it_started_with() {
        // The capacity property the pairing exists to hold.
        let (socket, _rx) = live(&[1, 2, 3], 16);
        let mut sockets = vec![socket];
        let held = vec![sockets[0].held.clone()];
        // Five wanted, three held — two arrivals cannot be funded.
        let plan = plan_depth20_minute(&held, &want(&[3, 4, 5, 6, 7]));
        apply_depth20_plan(&mut sockets, &plan);
        assert_eq!(
            sockets[0].held.len(),
            3,
            "the socket grew past its dialed size: {:?}",
            sockets[0].held
        );
    }
}

#[cfg(test)]
mod adversarial_tests {
    use super::*;
    use crate::depth20_layout::Depth20Socket as DesiredSocket;
    use tickvault_common::types::ExchangeSegment;

    fn ins(id: u64) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment: ExchangeSegment::NseFno,
        }
    }

    fn layout(sockets: Vec<Vec<u64>>) -> Depth20Layout {
        Depth20Layout {
            sockets: sockets
                .into_iter()
                .map(|ids| DesiredSocket {
                    underlying: None,
                    instruments: ids.into_iter().map(ins).collect(),
                })
                .collect(),
            ..Depth20Layout::default()
        }
    }

    /// A duplicate subscribe is answered with an 804, which is Fatal — the
    /// connection drops and does not come back this session.
    ///
    /// The first version of this module shipped without deduping, with
    /// eleven tests passing. It was found by attacking the output, not by
    /// reading the code.
    #[test]
    fn a_repeated_instrument_in_the_desired_set_is_never_subscribed_twice() {
        // Shapes must AGREE for the socket to be paired at all, so the
        // duplicate sits alongside a real third instrument: two held, three
        // wanted of which two are distinct.
        let held = vec![vec![ins(1), ins(2)]];
        let plan = plan_depth20_minute(&held, &layout(vec![vec![9, 9, 8]]));
        assert!(
            !plan.is_quiet(),
            "the socket was not paired, so nothing was tested"
        );
        let takes: Vec<u64> = plan.sockets[0]
            .swaps
            .iter()
            .map(|(_, t)| t.security_id)
            .collect();
        let mut uniq = takes.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(
            uniq.len(),
            takes.len(),
            "the same instrument was subscribed twice: {takes:?} — an 804 and a dead connection"
        );
    }

    #[test]
    fn a_repeated_instrument_already_held_is_never_unsubscribed_twice() {
        // Spending two slots to free one ends the minute below dialed size.
        // Two distinct held (1 repeated, and 2); three distinct wanted.
        let held = vec![vec![ins(1), ins(1), ins(2)]];
        let plan = plan_depth20_minute(&held, &layout(vec![vec![8, 9]]));
        assert!(
            !plan.is_quiet(),
            "the socket was not paired, so nothing was tested"
        );
        let releases: Vec<u64> = plan.sockets[0]
            .swaps
            .iter()
            .map(|(r, _)| r.security_id)
            .collect();
        let mut uniq = releases.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), releases.len(), "released twice: {releases:?}");
    }

    /// THE MISALIGNMENT. `plan_pool` shards a depth set into
    /// `len.div_ceil(connections)` chunks, so a layout that is not exactly
    /// 250 instruments puts different content on wire socket N than layout
    /// socket N describes.
    ///
    /// Index-only pairing would then diff a socket against a set it barely
    /// shares anything with and swap nearly all of it — every minute, all
    /// session, while reporting healthy swap counts.
    /// The case index-only pairing gets WRONG, and this one is not subtle:
    /// two sockets already holding exactly what the layout wants, in the
    /// other order.
    ///
    /// By position each diffs against the other's set — six swaps to arrive
    /// at the arrangement already on the wire, repeated every minute for the
    /// session. By content both are recognised and nothing moves.
    ///
    /// Written after the first version of this test passed under BOTH
    /// pairings and therefore proved nothing; the paired-swap rule turns out
    /// to absorb most boundary shifts on its own, so a test has to reach
    /// past it to say anything about the matcher.
    #[test]
    fn two_sockets_holding_each_others_sets_are_recognised_not_churned() {
        let held = vec![
            vec![ins(1), ins(2), ins(3)],
            vec![ins(10), ins(11), ins(12), ins(13)],
        ];
        // Same two sets, opposite sockets.
        let want = layout(vec![vec![10, 11, 12, 13], vec![1, 2, 3]]);
        let plan = plan_depth20_minute(&held, &want);
        assert!(
            plan.is_quiet(),
            "both sockets already hold exactly what is wanted; {} swaps is pure churn: {plan:?}",
            plan.swap_count()
        );
    }

    #[test]
    fn no_two_wire_sockets_ever_diff_against_the_same_layout_socket() {
        // The loser would be told to take instruments the winner is already
        // taking — two subscribes for one instrument across two connections.
        let want = layout(vec![vec![1, 2, 3, 4]]);
        let held = vec![vec![ins(1), ins(2)], vec![ins(3), ins(4)]];
        let plan = plan_depth20_minute(&held, &want);
        let mut takes: Vec<(u64, u8)> = plan
            .sockets
            .iter()
            .flat_map(|s| {
                s.swaps
                    .iter()
                    .map(|(_, t)| (t.security_id, t.segment as u8))
            })
            .collect();
        let before = takes.len();
        takes.sort_unstable();
        takes.dedup();
        assert_eq!(takes.len(), before, "one instrument taken by two sockets");
    }

    #[test]
    fn a_complete_turnover_is_still_re_aimed_when_the_shape_agrees() {
        // The case pure overlap-matching would freeze forever: a movers
        // socket whose entire ranking changed shares nothing with what it
        // holds, and that is exactly when it most needs to move.
        let held = vec![(0..50).map(ins).collect::<Vec<_>>()];
        let want = layout(vec![(100..150).collect()]);
        assert_eq!(plan_depth20_minute(&held, &want).swap_count(), 50);
    }

    #[test]
    fn an_unrecognisable_socket_holds_position_rather_than_guessing() {
        // Different size AND nothing in common: there is no evidence for
        // what this socket is, and acting on none of it is the safe answer.
        let held = vec![vec![ins(1), ins(2), ins(3)]];
        let want = layout(vec![vec![90, 91]]);
        let plan = plan_depth20_minute(&held, &want);
        assert!(
            plan.is_quiet(),
            "guessed an identity it had no evidence for: {plan:?}"
        );
        assert_eq!(plan.sockets_left_alone, 1);
    }

    #[test]
    fn the_planner_is_deterministic_across_repeated_runs() {
        // A swap the guard refuses once is refused forever, so two records of
        // one minute must produce identical traffic.
        let held = vec![vec![ins(4), ins(1), ins(3)], vec![ins(9), ins(7), ins(8)]];
        let want = layout(vec![vec![1, 3, 5], vec![7, 8, 12]]);
        let a = plan_depth20_minute(&held, &want);
        for _ in 0..25 {
            assert_eq!(plan_depth20_minute(&held, &want), a);
        }
    }

    #[test]
    fn an_empty_wire_and_an_empty_layout_are_both_quiet() {
        assert!(plan_depth20_minute(&[], &layout(vec![])).is_quiet());
        assert!(plan_depth20_minute(&[vec![]], &layout(vec![])).is_quiet());
        assert!(plan_depth20_minute(&[], &layout(vec![vec![1]])).is_quiet());
    }

    #[test]
    fn a_full_five_socket_session_shape_pairs_one_to_one() {
        // The real arrangement: 5 sockets x 50, shapes agree everywhere.
        let held: Vec<Vec<SubscribeInstrument>> = (0..5)
            .map(|s| (s * 50..s * 50 + 50).map(ins).collect())
            .collect();
        let want = layout((0..5).map(|s| (s * 50..s * 50 + 50).collect()).collect());
        assert!(
            plan_depth20_minute(&held, &want).is_quiet(),
            "an unchanged full session produced traffic"
        );
    }
}
