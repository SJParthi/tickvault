//! A depth-200 swap whose new contract never starts streaming
//! (scope lock 2026-10-13, "A SILENT DEPTH-200 SWAP IS RESUBSCRIBED IN PLACE
//! AT 90 s").
//!
//! # What went wrong
//!
//! A depth-200 socket changes contract in place (`LiveSubscriptionCommand::Swap`:
//! code 25 for the old contract, then 23 for the new one). On 2026-10-13 two
//! sockets swapped and then delivered nothing for 900 s, until the
//! frame-silence backstop redialled them: Dhan never started the new contract,
//! and about 15 minutes of 200-level depth was missed each time.
//!
//! The first-packet tracker (`depth_first_packet` in the app crate) did not see
//! it. It measures LATENCY, keyed on the contract rather than the socket, so it
//! refuses to watch a contract any depth socket held or dropped in the last ten
//! minutes (a ghost on another socket would fake a 0 ms sample), and the next
//! swap on the same socket forgets the previous stamp. Depth-200 rotates among
//! a few near-the-money strikes every minute, so the cases that matter fall in
//! exactly those two gaps.
//!
//! # What this module does
//!
//! It answers the silence question per SOCKET:
//!
//! 1. After a successful depth-200 swap, the connection task arms
//!    [`SilentSwapWatch`] for the NEW contract. That stores one packed word,
//!    the composite `(security_id, segment)` key, in this socket's slot of
//!    [`SilentSwapRegister`].
//! 2. The socket READ task calls [`note_depth200_frame`] for every frame it
//!    reads, BEFORE the frame is handed to the WAL and the ring. On a
//!    depth-200 socket it reads the 12-byte depth header of the frame's first
//!    packet; a bid or ask packet for exactly the awaited contract clears the
//!    slot. It runs before the sink on purpose: a frame the drain later SHEDS
//!    or the ring REFUSES (it goes only to the WAL) never reaches the drain's
//!    parser, so a drain-side check would read a flowing contract as silent
//!    during a shed and send a false resend (found by review, 2026-10-13; the
//!    cross-feed silence check reads the read task's stamp for the same reason).
//! 3. On its existing one-second idle tick the connection task asks
//!    [`SilentSwapWatch::poll`]. If the slot is still armed
//!    `[dhan_universe] depth200_silent_swap_resubscribe_secs` (90 s to start)
//!    after the swap, it gets the contract
//!    back once, and sends code 25 then code 23 for it on the live socket
//!    (never a redial). Still silent after that, nothing new happens: the
//!    900 s frame-silence backstop is the fallback. When the slot clears, the
//!    same poll records the subscribe-to-first-packet time.
//!
//! # Complexity
//!
//! * [`note_depth200_frame`]: the socket read task, once per FRAME. One
//!   endpoint compare (every other socket kind returns there), one
//!   bounds-checked index and one acquire load while nothing is awaited; while
//!   a swap is awaited, a fixed-offset header read, one compare and at most
//!   one compare-and-swap, plus one clock-anchor load on the one frame that
//!   clears it. O(1), no allocation, no lock, no metric.
//! * [`SilentSwapWatch::arm`] / [`SilentSwapWatch::poll`]: the connection
//!   task, once per swap and once per idle tick. O(1). The poll records one
//!   histogram sample per cleared swap; the production recorder can allocate
//!   a bucket block about once every 64 samples (the same as
//!   `depth_first_packet`), never per frame.
//!
//! # Honest limits
//!
//! * Only the first packet of a frame is read. A frame that stacks a ghost of
//!   the old contract ahead of the new one does not clear the slot; the next
//!   frame of a flowing contract does.
//! * The poll has no market-hours gate of its own. Swaps are only planned in
//!   session, so outside it nothing is armed; a swap in the last 90 s of the
//!   session can still be resent just after the close.
//! * A second swap within 90 s replaces the watch, so a socket that swaps every
//!   minute onto contracts Dhan never starts is never resent; the backstop
//!   still covers it.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tracing::{info, warn};

use tickvault_common::config::{
    DEPTH200_SILENT_SWAP_RESUBSCRIBE_DEFAULT_SECS, DEPTH200_SILENT_SWAP_RESUBSCRIBE_MAX_SECS,
    DEPTH200_SILENT_SWAP_RESUBSCRIBE_MIN_SECS,
};
use tickvault_common::constants::{DEEP_DEPTH_FEED_CODE_ASK, DEEP_DEPTH_FEED_CODE_BID};
use tickvault_common::error_code::ErrorCode;

use super::pool_budget::{ConnectionSlot, DhanEndpointType};
use super::pool_supervisor::{GHOST_REDIAL_SLOTS, SubscribeInstrument};
use crate::parser::depth::parse_depth_header;

/// How long a freshly swapped-in depth-200 contract may stay silent on its
/// socket before the subscription is sent again, in seconds. Set once at boot
/// from `[dhan_universe] depth200_silent_swap_resubscribe_secs`
/// ([`set_resubscribe_after_secs`]); 90 s until then. 90 s is the owner's
/// STARTING value (2026-10-13 decision card); lowering it once the measured
/// first-packet p99 supports it is a config change, not a code change.
static RESUBSCRIBE_AFTER_SECS: AtomicU64 =
    AtomicU64::new(DEPTH200_SILENT_SWAP_RESUBSCRIBE_DEFAULT_SECS);

/// Installs the configured wait, clamped to the validated bounds (config
/// validation already refuses a value outside them at boot; the clamp only
/// keeps a direct caller honest). Returns the value in force. O(1).
pub fn set_resubscribe_after_secs(secs: u64) -> u64 {
    let secs = secs.clamp(
        DEPTH200_SILENT_SWAP_RESUBSCRIBE_MIN_SECS,
        DEPTH200_SILENT_SWAP_RESUBSCRIBE_MAX_SECS,
    );
    RESUBSCRIBE_AFTER_SECS.store(secs, Ordering::Relaxed);
    secs
}

/// The wait in force. One relaxed load, read once per swap.
#[must_use]
pub fn resubscribe_after() -> Duration {
    Duration::from_secs(RESUBSCRIBE_AFTER_SECS.load(Ordering::Relaxed))
}

/// Counter, labelled `outcome`: what happened to each silent swap.
/// Local `/metrics` only: no EMF name, no alarm (scope lock 2026-10-13).
pub const SILENT_SWAP_RESUBSCRIBE_METRIC: &str = "tv_depth200_silent_swap_resubscribe_total";

/// The first-packet latency histogram the app's `depth_first_packet` module
/// already publishes (same name, pinned equal by an app test). This module
/// adds samples labelled `basis = "depth200_socket"`: milliseconds from a
/// depth-200 swap's subscribe to the new contract's first packet ON THAT
/// SOCKET, for every swap (the contract-keyed tracker refuses many depth-200
/// swaps). Local `/metrics` only; the name is not EMF-selected.
pub const FIRST_PACKET_LATENCY_METRIC: &str = "tv_depth_first_packet_latency_ms";

/// Every `outcome` label of [`SILENT_SWAP_RESUBSCRIBE_METRIC`], so each series
/// exists at zero from boot.
pub const SILENT_SWAP_OUTCOMES: [&str; 5] = [
    "sent",
    "refused_halted_805",
    "refused_not_held",
    "wire_failed",
    "recovered",
];

/// Set on every armed word, so an armed slot is never 0 (0 means "nothing
/// awaited"), whatever the id and segment.
const ARMED_BIT: u64 = 1 << 63;

/// Largest id the packed word can carry: 55 bits, so `id << 8` stays below
/// [`ARMED_BIT`]. Dhan ids are 32-bit today; a larger id is not watched.
const MAX_PACKABLE_ID: u64 = (1 << 55) - 1;

/// The armed word for `(security_id, segment_code)`, or `None` for an id too
/// large to pack. Keyed on the composite: the same id in another segment is a
/// different word (I-P1-11).
#[must_use]
pub const fn pack(security_id: u64, segment_code: u8) -> Option<u64> {
    if security_id > MAX_PACKABLE_ID {
        return None;
    }
    Some(ARMED_BIT | (security_id << 8) | segment_code as u64)
}

/// One awaited contract per connection slot, when it was armed, and the
/// measured first-packet time once it arrived.
///
/// Each slot has ONE writer that arms it (the slot's connection task) and one
/// that clears it (the socket read task, by compare-and-swap from the exact
/// armed word). The arm instant is stored before the word with Release, and
/// read after an Acquire load of the word, so the reader pairs a word with its
/// own arm instant (a re-arm of the SAME contract in between can only move it
/// later). The measured time is handed back to the connection task's poll
/// through `elapsed_nanos` (-1 = none), so no metric is touched per frame.
#[doc(hidden)]
pub struct SilentSwapRegister {
    awaiting: [AtomicU64; GHOST_REDIAL_SLOTS],
    armed_at_nanos: [AtomicI64; GHOST_REDIAL_SLOTS],
    elapsed_nanos: [AtomicI64; GHOST_REDIAL_SLOTS],
}

impl SilentSwapRegister {
    /// An empty register: nothing awaited on any slot.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            awaiting: [const { AtomicU64::new(0) }; GHOST_REDIAL_SLOTS],
            armed_at_nanos: [const { AtomicI64::new(0) }; GHOST_REDIAL_SLOTS],
            elapsed_nanos: [const { AtomicI64::new(-1) }; GHOST_REDIAL_SLOTS],
        }
    }

    /// Clears the slot if a packet carries exactly the awaited contract, and
    /// returns the nanoseconds from the arm to `received_at_nanos` on the one
    /// packet that clears it (`None` on every other packet, and for a receipt
    /// stamped before the arm, which still clears the slot). The time is also
    /// kept for the connection task's poll. O(1), no allocation.
    pub fn note_packet(
        &self,
        connection_index: u8,
        security_id: u64,
        segment_code: u8,
        received_at_nanos: i64,
    ) -> Option<i64> {
        self.note_packet_with(connection_index, security_id, segment_code, || {
            received_at_nanos
        })
    }

    /// [`Self::note_packet`] with the receipt time read only on the one packet
    /// that clears the slot.
    fn note_packet_with(
        &self,
        connection_index: u8,
        security_id: u64,
        segment_code: u8,
        received_at_nanos: impl FnOnce() -> i64,
    ) -> Option<i64> {
        let idx = usize::from(connection_index);
        let slot = self.awaiting.get(idx)?;
        let armed = slot.load(Ordering::Acquire);
        if armed == 0 || pack(security_id, segment_code) != Some(armed) {
            return None;
        }
        let armed_at = self
            .armed_at_nanos
            .get(idx)
            .map_or(0, |a| a.load(Ordering::Relaxed));
        // From the exact armed word only: a newer arm of another contract in
        // between is never cleared by a packet for this one, and exactly one
        // packet wins the claim.
        slot.compare_exchange(armed, 0, Ordering::Relaxed, Ordering::Relaxed)
            .ok()?;
        let elapsed = received_at_nanos()
            .checked_sub(armed_at)
            .filter(|e| *e >= 0 && armed_at > 0);
        if let Some(cell) = self.elapsed_nanos.get(idx) {
            cell.store(elapsed.unwrap_or(-1), Ordering::Relaxed);
        }
        elapsed
    }

    /// The read task's half for one raw frame: on a depth-200 slot, read the
    /// 12-byte header of the frame's first packet (fixed offsets) and clear
    /// the slot if it is a bid or ask packet for the awaited contract. O(1).
    pub fn note_frame(&self, connection_index: u8, frame: &[u8], received: Instant) {
        let Some(slot) = self.awaiting.get(usize::from(connection_index)) else {
            return;
        };
        if slot.load(Ordering::Acquire) == 0 {
            return;
        }
        let Ok(header) = parse_depth_header(frame) else {
            return;
        };
        if header.response_code != DEEP_DEPTH_FEED_CODE_BID
            && header.response_code != DEEP_DEPTH_FEED_CODE_ASK
        {
            return;
        }
        let _ = self.note_packet_with(
            connection_index,
            header.security_id,
            header.exchange_segment_code,
            || tickvault_storage::ws_frame_spill::receipt_nanos_from(received),
        );
    }

    /// Takes the first-packet time the read task left for this slot.
    fn take_elapsed(&self, connection_index: u8) -> Option<i64> {
        let cell = self.elapsed_nanos.get(usize::from(connection_index))?;
        let v = cell.swap(-1, Ordering::Relaxed);
        (v >= 0).then_some(v)
    }

    fn store(&self, connection_index: u8, word: u64, armed_at_nanos: i64) {
        let idx = usize::from(connection_index);
        if let (Some(slot), Some(at), Some(el)) = (
            self.awaiting.get(idx),
            self.armed_at_nanos.get(idx),
            self.elapsed_nanos.get(idx),
        ) {
            el.store(-1, Ordering::Relaxed);
            at.store(armed_at_nanos, Ordering::Relaxed);
            slot.store(word, Ordering::Release);
        }
    }

    fn holds(&self, connection_index: u8, word: u64) -> bool {
        self.awaiting
            .get(usize::from(connection_index))
            .is_some_and(|slot| slot.load(Ordering::Relaxed) == word)
    }
}

impl Default for SilentSwapRegister {
    fn default() -> Self {
        Self::new()
    }
}

/// The process's one register.
static REGISTER: SilentSwapRegister = SilentSwapRegister::new();

/// Nanoseconds in a millisecond, as the float the histogram takes.
const NANOS_PER_MILLI_F64: f64 = 1_000_000.0;

/// The socket read task's half: called for EVERY frame read on every socket,
/// before the frame is handed to the sink, so a frame the drain later sheds
/// or the ring refuses still counts. Returns at once for anything but a
/// depth-200 slot.
///
/// O(1), no allocation, no lock, no metric: one endpoint compare; on a
/// depth-200 slot one bounds-checked index and one acquire load while nothing
/// is awaited (almost every frame), plus a fixed-offset header read, one
/// compare and at most one compare-and-swap while a swap is awaited.
pub fn note_depth200_frame(slot: ConnectionSlot, frame: &[u8], received: Instant) {
    if slot.endpoint == DhanEndpointType::Depth200 {
        REGISTER.note_frame(slot.global_index, frame, received);
    }
}

/// Records one cleared swap's first-packet time. Called from the connection
/// task's poll, once per cleared swap, never per frame.
fn record_first_packet(elapsed_nanos: i64) {
    metrics::histogram!(FIRST_PACKET_LATENCY_METRIC, "basis" => "depth200_socket")
        .record(elapsed_nanos as f64 / NANOS_PER_MILLI_F64);
}

/// Registers every outcome of [`SILENT_SWAP_RESUBSCRIBE_METRIC`] at zero.
/// Called once at boot.
pub fn pre_register_silent_swap_counters() {
    for outcome in SILENT_SWAP_OUTCOMES {
        metrics::counter!(SILENT_SWAP_RESUBSCRIBE_METRIC, "outcome" => outcome).increment(0);
    }
}

/// Counts one outcome. Only label values from [`SILENT_SWAP_OUTCOMES`] are
/// passed, all `'static` literals.
pub fn count_outcome(outcome: &'static str) {
    metrics::counter!(SILENT_SWAP_RESUBSCRIBE_METRIC, "outcome" => outcome).increment(1);
}

/// What the connection task should do with a due resend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResendGate {
    /// Send code 25 then code 23 for the contract.
    Send,
    /// An 805 has halted every depth change this process (`ROTATION_HALTED`).
    RefusedHalted805,
    /// The socket's guard no longer names the contract (a later swap moved
    /// it, or the guard was emptied).
    RefusedNotHeld,
}

impl ResendGate {
    /// The outcome label for a refusal; `None` for [`Self::Send`].
    #[must_use]
    pub const fn refusal_label(self) -> Option<&'static str> {
        match self {
            Self::Send => None,
            Self::RefusedHalted805 => Some("refused_halted_805"),
            Self::RefusedNotHeld => Some("refused_not_held"),
        }
    }
}

/// Decides whether a due resend may reach the wire. The 805 breaker first,
/// then the guard. Pure, O(1).
#[must_use]
pub const fn resend_gate(rotation_halted: bool, guard_holds: bool) -> ResendGate {
    if rotation_halted {
        ResendGate::RefusedHalted805
    } else if !guard_holds {
        ResendGate::RefusedNotHeld
    } else {
        ResendGate::Send
    }
}

#[derive(Debug, Clone, Copy)]
struct Armed {
    instrument: SubscribeInstrument,
    word: u64,
    since: Instant,
    after: Duration,
    resent: bool,
}

/// The connection task's half: one per drain of one depth-200 socket.
#[derive(Debug)]
pub struct SilentSwapWatch {
    connection_index: u8,
    armed: Option<Armed>,
}

impl SilentSwapWatch {
    /// A watch for one connection slot, with the slot cleared: nothing a
    /// previous connection on this slot armed is acted on.
    #[must_use]
    pub fn new(connection_index: u8) -> Self {
        Self::new_in(&REGISTER, connection_index)
    }

    /// [`Self::new`] on a given register (tests).
    #[doc(hidden)]
    #[must_use]
    pub fn new_in(register: &SilentSwapRegister, connection_index: u8) -> Self {
        register.store(connection_index, 0, 0);
        Self {
            connection_index,
            armed: None,
        }
    }

    /// Starts watching `instrument`, swapped onto this socket at `now`.
    /// Replaces any earlier watch: a second swap before the resend cancels
    /// the first. Called BEFORE the subscribe is written, so a packet the
    /// read task sees during the write already counts. O(1).
    pub fn arm(&mut self, instrument: SubscribeInstrument, now: Instant) {
        let armed_at = tickvault_storage::ws_frame_spill::receipt_nanos_from(Instant::now());
        self.arm_in(&REGISTER, instrument, now, armed_at, resubscribe_after());
    }

    /// [`Self::arm`] on a given register (tests).
    #[doc(hidden)]
    pub fn arm_in(
        &mut self,
        register: &SilentSwapRegister,
        instrument: SubscribeInstrument,
        now: Instant,
        armed_at_nanos: i64,
        after: Duration,
    ) {
        let Some(word) = pack(instrument.security_id, instrument.segment.binary_code()) else {
            // Unreachable for a Dhan id; not watched rather than mis-keyed.
            self.cancel_in(register);
            return;
        };
        register.store(self.connection_index, word, armed_at_nanos);
        self.armed = Some(Armed {
            instrument,
            word,
            since: now,
            after,
            resent: false,
        });
    }

    /// Stops watching (a swap that failed on the wire). O(1).
    pub fn cancel(&mut self) {
        self.cancel_in(&REGISTER);
    }

    /// [`Self::cancel`] on a given register (tests).
    #[doc(hidden)]
    pub fn cancel_in(&mut self, register: &SilentSwapRegister) {
        if self.armed.take().is_some() {
            register.store(self.connection_index, 0, 0);
        }
    }

    /// The idle-tick check. Returns the contract ONCE, when it has stayed
    /// silent for the configured wait since its swap; `None`
    /// otherwise. A packet that clears the slot ends the watch, and after a
    /// resend it is counted `recovered`. O(1).
    pub fn poll(&mut self, now: Instant) -> Option<SubscribeInstrument> {
        self.poll_in(&REGISTER, now)
    }

    /// [`Self::poll`] on a given register (tests).
    #[doc(hidden)]
    pub fn poll_in(
        &mut self,
        register: &SilentSwapRegister,
        now: Instant,
    ) -> Option<SubscribeInstrument> {
        let armed = self.armed.as_mut()?;
        if !register.holds(self.connection_index, armed.word) {
            // The read task saw the contract: it is streaming.
            if let Some(elapsed) = register.take_elapsed(self.connection_index) {
                record_first_packet(elapsed);
            }
            if armed.resent {
                count_outcome("recovered");
                info!(
                    source = "depth200_silent_swap_recovered",
                    connection_index = self.connection_index,
                    security_id = armed.instrument.security_id,
                    segment = armed.instrument.segment.as_str(),
                    "a depth-200 contract that was silent after its swap is streaming \
                     after the in-place resubscribe"
                );
            }
            self.armed = None;
            return None;
        }
        if armed.resent || now.saturating_duration_since(armed.since) < armed.after {
            return None;
        }
        // Once per swap: the watch stays armed so a later packet still counts
        // as `recovered`, but it never returns the contract again.
        armed.resent = true;
        Some(armed.instrument)
    }

    /// Whether a contract is being watched. Tests and logs.
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.armed.is_some()
    }
}

/// One coded warning per resend decision, never per packet.
pub fn warn_resend(
    connection_index: u8,
    pool_index: u8,
    instrument: SubscribeInstrument,
    outcome: &'static str,
) {
    warn!(
        code = ErrorCode::WsGapSubscriptionBatching.code_str(),
        source = "depth200_silent_swap_resubscribe",
        connection_index,
        pool_index,
        security_id = instrument.security_id,
        segment = instrument.segment.as_str(),
        outcome,
        silent_secs = RESUBSCRIBE_AFTER_SECS.load(Ordering::Relaxed),
        "a depth-200 contract sent nothing after its swap; its subscription is sent again \
         on the live socket (unsubscribe, then subscribe), once, without a redial. If it \
         stays silent the 900 s frame-silence backstop redials the socket."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::types::ExchangeSegment;

    fn inst(id: u64, segment: ExchangeSegment) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment,
        }
    }

    const SLOT: u8 = 7;
    /// Wall arm instant for the register (any positive value).
    const ARM_NANOS: i64 = 1_779_355_000_000_000_000;
    /// The owner's starting wait.
    const AFTER: Duration = Duration::from_secs(90);

    fn at(t0: Instant, secs: u64) -> Instant {
        t0 + Duration::from_secs(secs)
    }

    #[test]
    fn test_regression_silent_swap_resends_once_at_90_seconds() {
        let reg = SilentSwapRegister::new();
        let mut watch = SilentSwapWatch::new_in(&reg, SLOT);
        let t0 = Instant::now();
        let new = inst(51_234, ExchangeSegment::NseFno);
        watch.arm_in(&reg, new, t0, ARM_NANOS, AFTER);
        assert_eq!(watch.poll_in(&reg, at(t0, 1)), None);
        assert_eq!(watch.poll_in(&reg, at(t0, 89)), None, "not before 90 s");
        assert_eq!(watch.poll_in(&reg, at(t0, 90)), Some(new), "due at 90 s");
    }

    #[test]
    fn test_regression_silent_swap_no_resend_when_a_packet_arrived() {
        let reg = SilentSwapRegister::new();
        let mut watch = SilentSwapWatch::new_in(&reg, SLOT);
        let t0 = Instant::now();
        let new = inst(51_234, ExchangeSegment::NseFno);
        watch.arm_in(&reg, new, t0, ARM_NANOS, AFTER);
        let _ = reg.note_packet(
            SLOT,
            51_234,
            ExchangeSegment::NseFno.binary_code(),
            ARM_NANOS + 1,
        );
        assert_eq!(watch.poll_in(&reg, at(t0, 200)), None);
        assert!(!watch.is_armed(), "an arrival ends the watch");
    }

    #[test]
    fn test_regression_silent_swap_one_resend_per_swap_only() {
        let reg = SilentSwapRegister::new();
        let mut watch = SilentSwapWatch::new_in(&reg, SLOT);
        let t0 = Instant::now();
        let new = inst(9, ExchangeSegment::NseFno);
        watch.arm_in(&reg, new, t0, ARM_NANOS, AFTER);
        assert_eq!(watch.poll_in(&reg, at(t0, 90)), Some(new));
        for s in 91..2_000 {
            assert_eq!(watch.poll_in(&reg, at(t0, s)), None, "never twice per swap");
        }
        assert!(
            watch.is_armed(),
            "kept armed so a late arrival reads recovered"
        );
        let _ = reg.note_packet(
            SLOT,
            9,
            ExchangeSegment::NseFno.binary_code(),
            ARM_NANOS + 1,
        );
        assert_eq!(watch.poll_in(&reg, at(t0, 2_001)), None);
        assert!(!watch.is_armed());
        // A NEW swap gets its own single resend.
        watch.arm_in(&reg, new, at(t0, 3_000), ARM_NANOS, AFTER);
        assert_eq!(watch.poll_in(&reg, at(t0, 3_090)), Some(new));
    }

    #[test]
    fn test_regression_silent_swap_composite_key_other_segment_does_not_count() {
        let reg = SilentSwapRegister::new();
        let mut watch = SilentSwapWatch::new_in(&reg, SLOT);
        let t0 = Instant::now();
        let new = inst(27, ExchangeSegment::NseFno);
        watch.arm_in(&reg, new, t0, ARM_NANOS, AFTER);
        // Same id, another segment: a different instrument (I-P1-11).
        let _ = reg.note_packet(
            SLOT,
            27,
            ExchangeSegment::NseEquity.binary_code(),
            ARM_NANOS + 1,
        );
        let _ = reg.note_packet(
            SLOT,
            27,
            ExchangeSegment::BseFno.binary_code(),
            ARM_NANOS + 1,
        );
        // Same contract on ANOTHER socket does not count for this one.
        let _ = reg.note_packet(
            SLOT + 1,
            27,
            ExchangeSegment::NseFno.binary_code(),
            ARM_NANOS + 1,
        );
        assert_eq!(watch.poll_in(&reg, at(t0, 90)), Some(new));
    }

    #[test]
    fn test_regression_silent_swap_second_swap_before_90s_cancels_the_first() {
        let reg = SilentSwapRegister::new();
        let mut watch = SilentSwapWatch::new_in(&reg, SLOT);
        let t0 = Instant::now();
        let first = inst(100, ExchangeSegment::NseFno);
        let second = inst(200, ExchangeSegment::NseFno);
        watch.arm_in(&reg, first, t0, ARM_NANOS, AFTER);
        watch.arm_in(&reg, second, at(t0, 60), ARM_NANOS, AFTER);
        // The first contract's packet no longer clears anything.
        let _ = reg.note_packet(
            SLOT,
            100,
            ExchangeSegment::NseFno.binary_code(),
            ARM_NANOS + 1,
        );
        assert_eq!(
            watch.poll_in(&reg, at(t0, 90)),
            None,
            "the first is cancelled"
        );
        assert_eq!(watch.poll_in(&reg, at(t0, 149)), None);
        assert_eq!(watch.poll_in(&reg, at(t0, 150)), Some(second));
    }

    #[test]
    fn test_regression_silent_swap_refused_after_805() {
        assert_eq!(resend_gate(true, true), ResendGate::RefusedHalted805);
        assert_eq!(resend_gate(true, false), ResendGate::RefusedHalted805);
        assert_eq!(
            ResendGate::RefusedHalted805.refusal_label(),
            Some("refused_halted_805")
        );
    }

    #[test]
    fn test_regression_silent_swap_refused_when_the_guard_no_longer_holds() {
        assert_eq!(resend_gate(false, false), ResendGate::RefusedNotHeld);
        assert_eq!(
            ResendGate::RefusedNotHeld.refusal_label(),
            Some("refused_not_held")
        );
        assert_eq!(resend_gate(false, true), ResendGate::Send);
        assert_eq!(ResendGate::Send.refusal_label(), None);
    }

    #[test]
    fn test_regression_silent_swap_cancel_in_and_new_in_clear_the_slot() {
        let reg = SilentSwapRegister::new();
        let mut watch = SilentSwapWatch::new_in(&reg, SLOT);
        let t0 = Instant::now();
        watch.arm_in(&reg, inst(5, ExchangeSegment::NseFno), t0, ARM_NANOS, AFTER);
        watch.cancel_in(&reg);
        assert!(!watch.is_armed());
        assert_eq!(watch.poll_in(&reg, at(t0, 500)), None);
        // A fresh connection on the slot clears what the previous one armed.
        let mut old = SilentSwapWatch::new_in(&reg, SLOT);
        old.arm_in(&reg, inst(6, ExchangeSegment::NseFno), t0, ARM_NANOS, AFTER);
        let _fresh = SilentSwapWatch::new_in(&reg, SLOT);
        assert!(!reg.holds(
            SLOT,
            pack(6, ExchangeSegment::NseFno.binary_code()).unwrap_or(0)
        ));
    }

    #[test]
    fn test_regression_silent_swap_wait_is_the_configured_value_set_resubscribe_after_secs() {
        let reg = SilentSwapRegister::new();
        let mut watch = SilentSwapWatch::new_in(&reg, SLOT);
        let t0 = Instant::now();
        let new = inst(77, ExchangeSegment::NseFno);
        watch.arm_in(&reg, new, t0, ARM_NANOS, Duration::from_secs(30));
        assert_eq!(watch.poll_in(&reg, at(t0, 29)), None);
        assert_eq!(
            watch.poll_in(&reg, at(t0, 30)),
            Some(new),
            "a lowered wait applies"
        );
        // The installer clamps to the validated bounds.
        assert_eq!(
            set_resubscribe_after_secs(1),
            DEPTH200_SILENT_SWAP_RESUBSCRIBE_MIN_SECS
        );
        assert_eq!(
            set_resubscribe_after_secs(10_000),
            DEPTH200_SILENT_SWAP_RESUBSCRIBE_MAX_SECS
        );
        assert_eq!(set_resubscribe_after_secs(90), 90);
        assert_eq!(resubscribe_after(), Duration::from_secs(90));
    }

    #[test]
    fn test_regression_silent_swap_note_packet_measures_first_packet_latency_once() {
        let reg = SilentSwapRegister::new();
        let mut watch = SilentSwapWatch::new_in(&reg, SLOT);
        let code = ExchangeSegment::NseFno.binary_code();
        watch.arm_in(
            &reg,
            inst(5, ExchangeSegment::NseFno),
            Instant::now(),
            ARM_NANOS,
            AFTER,
        );
        assert_eq!(
            reg.note_packet(SLOT, 6, code, ARM_NANOS + 9),
            None,
            "another contract"
        );
        assert_eq!(
            reg.note_packet(SLOT, 5, code, ARM_NANOS + 2_500_000),
            Some(2_500_000)
        );
        assert_eq!(
            reg.note_packet(SLOT, 5, code, ARM_NANOS + 9_000_000),
            None,
            "once"
        );
        // A receipt before the arm clears the slot but is not a sample.
        watch.arm_in(
            &reg,
            inst(5, ExchangeSegment::NseFno),
            Instant::now(),
            ARM_NANOS,
            AFTER,
        );
        assert_eq!(reg.note_packet(SLOT, 5, code, ARM_NANOS - 1), None);
        assert!(!reg.holds(SLOT, pack(5, code).unwrap_or(0)));
    }

    #[test]
    fn pack_is_composite_nonzero_and_refuses_oversized_ids() {
        assert_ne!(pack(0, 0), Some(0), "an armed word is never 0");
        assert_ne!(pack(27, 1), pack(27, 2));
        assert_ne!(pack(1, 0), pack(0, 1));
        assert_eq!(pack(MAX_PACKABLE_ID + 1, 2), None);
        assert!(pack(MAX_PACKABLE_ID, 8).is_some());
        // Out-of-range slots are ignored, never a panic.
        let reg = SilentSwapRegister::new();
        let _ = reg.note_packet(u8::MAX, 1, 2, ARM_NANOS + 1);
        assert!(!reg.holds(u8::MAX, 0));
    }

    #[test]
    fn test_pre_register_silent_swap_counters_and_count_outcome_run() {
        pre_register_silent_swap_counters();
        for outcome in SILENT_SWAP_OUTCOMES {
            count_outcome(outcome);
        }
        assert_eq!(SILENT_SWAP_OUTCOMES.len(), 5);
    }

    #[test]
    fn test_warn_resend_names_every_outcome_without_panicking() {
        for outcome in SILENT_SWAP_OUTCOMES {
            warn_resend(SLOT, 2, inst(1, ExchangeSegment::NseFno), outcome);
        }
    }

    /// One depth packet as it arrives on the wire: the 12-byte header (length,
    /// feed code, segment, security id, row count) and no levels.
    fn depth_frame(security_id: u32, segment: ExchangeSegment, feed_code: u8) -> Vec<u8> {
        let mut p = vec![0_u8; 12];
        p[0..2].copy_from_slice(&12_u16.to_le_bytes());
        p[2] = feed_code;
        p[3] = segment.binary_code();
        p[4..8].copy_from_slice(&security_id.to_le_bytes());
        p
    }

    fn depth200_slot(global_index: u8) -> ConnectionSlot {
        ConnectionSlot {
            account: super::super::pool_budget::DhanAccount::Primary,
            endpoint: DhanEndpointType::Depth200,
            pool_index: 0,
            global_index,
        }
    }

    #[test]
    fn test_note_depth200_frame_and_the_production_watch_poll_in_is_armed() {
        // The process register, on slot 31 (unused by the other tests).
        let mut w = SilentSwapWatch::new(31);
        assert!(!w.is_armed());
        w.arm(inst(1, ExchangeSegment::NseFno), Instant::now());
        assert!(w.is_armed());
        // Another socket kind with the same index never touches the watch.
        let mut main = depth200_slot(31);
        main.endpoint = DhanEndpointType::MainFeed;
        let frame = depth_frame(1, ExchangeSegment::NseFno, DEEP_DEPTH_FEED_CODE_BID);
        note_depth200_frame(main, &frame, Instant::now());
        assert!(w.poll(Instant::now()).is_none());
        assert!(w.is_armed());
        note_depth200_frame(depth200_slot(31), &frame, Instant::now());
        assert!(w.poll(Instant::now()).is_none());
        assert!(!w.is_armed(), "the read task's frame ended the watch");
        w.arm(inst(2, ExchangeSegment::NseFno), Instant::now());
        w.cancel();
        assert!(!w.is_armed());
        // `poll_in` on a fresh register with nothing armed is a no-op.
        let reg = SilentSwapRegister::new();
        let mut idle = SilentSwapWatch::new_in(&reg, 30);
        assert_eq!(idle.poll_in(&reg, Instant::now()), None);
    }

    /// Review HIGH, 2026-10-13: a frame the drain SHEDS, or the ring refuses
    /// (WAL only), is never parsed by the drain, so a drain-side check read a
    /// flowing contract as silent and sent a false 25/23 after 90 s of shed.
    /// The read task clears the watch before the sink, whatever the sink then
    /// does with the frame (the cross-feed check's
    /// `a_depth_shed_never_triggers_a_cross_feed_redial`, mirrored).
    #[test]
    fn test_regression_silent_swap_a_depth_shed_never_triggers_a_false_resend() {
        let reg = SilentSwapRegister::new();
        let mut watch = SilentSwapWatch::new_in(&reg, SLOT);
        let t0 = Instant::now();
        let new = inst(70_002, ExchangeSegment::NseFno);
        watch.arm_in(&reg, new, t0, ARM_NANOS, AFTER);
        // A whole shed: the drain parses nothing, the read task sees frames.
        for code in [DEEP_DEPTH_FEED_CODE_BID, DEEP_DEPTH_FEED_CODE_ASK] {
            let frame = depth_frame(70_002, ExchangeSegment::NseFno, code);
            reg.note_frame(SLOT, &frame, t0);
        }
        for secs in [90, 200, 900] {
            assert_eq!(watch.poll_in(&reg, at(t0, secs)), None, "no false resend");
        }
        // The read side runs before the sink in the socket read arm, on every
        // frame, whatever the sink's outcome.
        let src = include_str!("pool_supervisor.rs");
        let prod = src
            .split(concat!("#[cfg(", "test)]"))
            .next()
            .unwrap_or_default();
        let arm = prod
            .split("SocketEvent::Frame(frame) => {")
            .nth(1)
            .expect("the read arm");
        let note = arm
            .find("note_depth200_frame(")
            .expect("the read side is wired");
        let accept = arm.find("sink.accept(frame)").expect("the sink call");
        assert!(note < accept, "the watch is fed before the sink");
    }

    #[test]
    fn test_note_frame_reads_only_bid_or_ask_headers_of_the_awaited_contract() {
        let reg = SilentSwapRegister::new();
        let mut watch = SilentSwapWatch::new_in(&reg, SLOT);
        let t0 = Instant::now();
        let new = inst(70_002, ExchangeSegment::NseFno);
        watch.arm_in(&reg, new, t0, ARM_NANOS, AFTER);
        // A disconnect packet, a short frame, another segment, another id and
        // another slot all leave the watch armed.
        let disconnect = depth_frame(70_002, ExchangeSegment::NseFno, 50);
        reg.note_frame(SLOT, &disconnect, t0);
        reg.note_frame(SLOT, &[0_u8; 11], t0);
        let bse = depth_frame(70_002, ExchangeSegment::BseFno, DEEP_DEPTH_FEED_CODE_BID);
        reg.note_frame(SLOT, &bse, t0);
        let other = depth_frame(70_003, ExchangeSegment::NseFno, DEEP_DEPTH_FEED_CODE_BID);
        reg.note_frame(SLOT, &other, t0);
        let ok = depth_frame(70_002, ExchangeSegment::NseFno, DEEP_DEPTH_FEED_CODE_ASK);
        reg.note_frame(SLOT + 1, &ok, t0);
        assert_eq!(watch.poll_in(&reg, at(t0, 90)), Some(new));
    }
}
