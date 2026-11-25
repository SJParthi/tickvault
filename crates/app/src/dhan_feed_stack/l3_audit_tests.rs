//! Audit L3 (2026-10-26): the seal ledger counts rescued seals, the
//! connected-but-silent check runs in the Muhurat session, and a refused
//! dial precondition is waited on instead of ending the lane.

use super::*;

// Regression: 2026-10-26 — the seal-accounting `debug_assert` compared bars
// to emitted + dropped, so a seal rescued to disk (neither) panicked the
// drain in a debug build.
#[test]
fn test_regression_seal_tally_counts_rescued_seals() {
    let tally = SealTally {
        emitted: 1,
        dropped: 0,
        rescued: 2,
    };
    assert!(tally.accounts_for(3), "a rescued seal is accounted for");
    assert!(!tally.accounts_for(2), "a bar too few is not");
    assert!(!tally.accounts_for(4), "a bar unaccounted for is not");
    assert!(SealTally::default().accounts_for(0));
}

// Regression: 2026-10-26 — a Muhurat date is an NSE holiday, so the RISK-GAP-03
// silence check was off for the whole evening session.
#[test]
fn test_regression_muhurat_session_runs_the_silence_check() {
    let mid_session = 18 * 3_600 + 40 * 60; // 18:40 IST
    assert!(muhurat_silence_check_due(mid_session, true));
    assert!(
        !muhurat_silence_check_due(mid_session, false),
        "an ordinary evening never runs it"
    );
}

#[test]
fn test_muhurat_silence_check_due_window_bounds() {
    let start = MUHURAT_CONTINUOUS_START_SECS_OF_DAY_IST;
    let end = u64::from(tickvault_common::constants::MUHURAT_PERSIST_END_SECS_OF_DAY_IST);
    assert_eq!(start, 18 * 3_600 + 15 * 60, "18:15, the pre-open excluded");
    assert!(!muhurat_silence_check_due(start - 1, true));
    assert!(muhurat_silence_check_due(start, true));
    assert!(muhurat_silence_check_due(end - 1, true));
    assert!(!muhurat_silence_check_due(end, true), "19:30 is outside");
    assert!(
        !muhurat_silence_check_due(10 * 3_600, true),
        "the Muhurat arm never opens the regular day; that is the calendar's gate"
    );
}

#[test]
fn test_lane_refusal_retry_secs_doubles_to_sixty_and_holds() {
    let ladder: Vec<u64> = (0..8).map(lane_refusal_retry_secs).collect();
    assert_eq!(ladder, vec![5, 10, 20, 40, 60, 60, 60, 60]);
    assert_eq!(
        lane_refusal_retry_secs(u32::MAX),
        LANE_REFUSAL_RETRY_MAX_SECS
    );
}

#[test]
fn test_in_token_manager_grace_covers_the_auth_wait_only() {
    let budget = TOKEN_MANAGER_WAIT_ATTEMPTS * TOKEN_MANAGER_WAIT_INTERVAL_SECS;
    assert!(in_token_manager_grace(DialRefusal::NoTokenManager, 0));
    assert!(in_token_manager_grace(
        DialRefusal::NoTokenManager,
        budget - 1
    ));
    assert!(!in_token_manager_grace(DialRefusal::NoTokenManager, budget));
    assert!(
        !in_token_manager_grace(DialRefusal::LockNotHeld, 0),
        "a missing lock is never the expected boot wait"
    );
}

// Regression: 2026-10-26 — the lane returned for good when no token manager
// registered inside the 300 s wait. It must still be waiting long after that,
// and leave only when a shutdown arrives.
#[tokio::test(start_paused = true)]
async fn test_regression_wait_for_dial_preconditions_never_gives_up() {
    // No test in this binary registers the global token manager, so the
    // wait sees `NoTokenManager` throughout.
    assert!(global_token_manager().is_none());
    let lock = Arc::new(AtomicBool::new(true));
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let task = {
        let lock = Arc::clone(&lock);
        let shutdown = Arc::clone(&shutdown);
        tokio::spawn(async move { wait_for_dial_preconditions(&lock, &shutdown, 1).await })
    };
    // Two hours: far past the 300 s authentication wait.
    for _ in 0..240 {
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
    }
    assert!(
        !task.is_finished(),
        "the lane must still be waiting, not refused"
    );
    shutdown.notify_one();
    let waited = task
        .await
        .expect("the wait task completes")
        .expect_err("only a shutdown ends the wait while nothing is registered");
    let budget = TOKEN_MANAGER_WAIT_ATTEMPTS * TOKEN_MANAGER_WAIT_INTERVAL_SECS;
    assert!(
        waited > 10 * budget,
        "it kept waiting far past the {budget}s authentication wait, got {waited}s"
    );
}
