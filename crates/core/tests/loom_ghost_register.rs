//! Loom model of the REAL ghost-unsubscribe register (audit H3, 2026-10-05).
//!
//! The other core loom files model hand-written copies of production code, so
//! a change to the real orderings cannot fail them. This one builds a
//! `GhostRegister` and calls its `request` and `take`, the exact methods
//! behind `request_ghost_unsubscribe` and `take_ghost_unsubscribe`. Under the
//! crate's `loom` feature the register's atomics are loom's (`crate::sync`),
//! so loom explores every interleaving of the drain (which arms requests) and
//! the connection task (which takes them).
//!
//! What it proves, in every interleaving:
//!
//! - a taken request is never TORN: the id always arrives with the segment it
//!   was stored with (I-P1-11, `security_id` alone is not unique);
//! - no request is lost and none is taken twice: requests armed equal
//!   requests taken;
//! - a request made while an earlier one is still pending is refused
//!   (`StillPending`), never written over the pending one.
//!
//! Run with:
//!   cargo test -p tickvault-core --features loom --test loom_ghost_register

#[cfg(feature = "loom")]
mod loom_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use loom::sync::Arc;
    use loom::thread;
    use tickvault_common::types::ExchangeSegment;
    use tickvault_core::websocket::pool_supervisor::{
        GHOST_RESEND_COOLDOWN_SECS, GhostRegister, GhostResendRefusal, SubscribeInstrument,
    };

    const SLOT: u8 = 3;
    /// NSE_FNO binary segment code.
    const FNO: u8 = 2;
    /// NSE_EQ binary segment code.
    const EQ: u8 = 1;
    const FIRST_AT: i64 = 1_000;
    const SECOND_AT: i64 = FIRST_AT + GHOST_RESEND_COOLDOWN_SECS;

    /// Each id was armed with exactly one segment; a taken pair must match.
    fn assert_not_torn(taken: &SubscribeInstrument) {
        let expected = match taken.security_id {
            1 => ExchangeSegment::from_byte(FNO),
            2 => ExchangeSegment::from_byte(EQ),
            other => panic!("taken an id that was never armed: {other}"),
        };
        assert_eq!(Some(taken.segment), expected, "torn pair {taken:?}");
    }

    /// The drain re-arms on its own thread while the connection task takes.
    ///
    /// Shaped this way on purpose. Measured: the mirror shape (the take on the
    /// spawned thread) explored exactly one execution. Assumed cause: loom 0.7
    /// tracks only the last access to each atomic, so a thread whose load is
    /// followed by its own store hides the race. With the take on the main
    /// thread loom explores both outcomes; the anti-vacuity asserts below fail
    /// if a loom change ever brings back a single execution.
    #[test]
    fn a_take_racing_a_new_request_never_tears_loses_or_duplicates() {
        static SECOND_ARMED: AtomicUsize = AtomicUsize::new(0);
        static SECOND_REFUSED: AtomicUsize = AtomicUsize::new(0);

        loom::model(|| {
            let register = Arc::new(GhostRegister::new());
            // The drain armed one request before the connection task woke.
            assert_eq!(register.request(SLOT, 1, FNO, FIRST_AT), Ok(()));

            // The drain sees the ghost again, a cooldown later.
            let drain_register = Arc::clone(&register);
            let drain = thread::spawn(move || drain_register.request(SLOT, 2, EQ, SECOND_AT));

            // Meanwhile the connection task takes on its idle tick.
            let first_take = register.take(SLOT);

            let second = drain.join().expect("drain thread");
            match second {
                Ok(()) => SECOND_ARMED.fetch_add(1, Ordering::Relaxed),
                Err(GhostResendRefusal::StillPending) => {
                    SECOND_REFUSED.fetch_add(1, Ordering::Relaxed)
                }
                Err(other) => panic!("unexpected refusal {other:?}"),
            };
            let leftover = register.take(SLOT);

            let taken: Vec<SubscribeInstrument> =
                [first_take, leftover].into_iter().flatten().collect();
            taken.iter().for_each(assert_not_torn);

            let armed = 1 + usize::from(second.is_ok());
            assert_eq!(taken.len(), armed, "armed {armed}, taken {taken:?}");
            // The first request is always the first one out.
            assert_eq!(taken.first().map(|t| t.security_id), Some(1));
            // Nothing is left pending once everything is taken.
            assert!(register.take(SLOT).is_none());
        });

        // ANTI-VACUITY: loom must have explored both outcomes of the race.
        assert!(
            SECOND_ARMED.load(Ordering::Relaxed) > 0,
            "no execution armed the second request"
        );
        assert!(
            SECOND_REFUSED.load(Ordering::Relaxed) > 0,
            "no execution refused it as pending"
        );
    }

    #[test]
    fn a_pending_request_is_never_overwritten_by_a_later_one() {
        loom::model(|| {
            let register = Arc::new(GhostRegister::new());
            assert_eq!(register.request(SLOT, 1, FNO, FIRST_AT), Ok(()));
            let drain_register = Arc::clone(&register);
            let drain = thread::spawn(move || drain_register.request(SLOT, 2, EQ, SECOND_AT));
            // Nothing takes, so the second request must be refused.
            assert_eq!(
                drain.join().expect("drain thread"),
                Err(GhostResendRefusal::StillPending)
            );
            let taken = register
                .take(SLOT)
                .expect("the first request is still pending");
            assert_eq!(taken.security_id, 1);
            assert_not_torn(&taken);
        });
    }
}
