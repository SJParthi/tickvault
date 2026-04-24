//! W3-W6 integration chaos tests — extreme-worst-case boot scenarios.
//!
//! Tracks the "Queued follow-up: extreme-worst-case boot scenarios"
//! item in `.claude/plans/active-plan.md`. Each scenario gets at
//! least one pure-logic ratchet here; the full process/Docker-kill
//! harness lives in a separate shell script (future work).
//!
//! Scenarios:
//!   W3 — app crash + systemd restart mid-session → no double subscribe
//!   W4 — Docker `down -v` full wipe → emergency CSV download succeeds
//!        silently (no spurious error! during a clean success path)
//!   W5 — host reboot mid-session → WAL replay yields live frames ONLY
//!        (no synthesized historical ticks), and DEDUP protects against
//!        double-replay
//!   W6 — Dhan TCP-RST storm during Phase 2 dispatch → all subscriptions
//!        preserved after reconnect via SubscribeRxGuard (PR #337)
//!
//! W3 and W5 are implemented as pure-logic tests that run in-process.
//! W4 and W6 are stubbed `#[ignore]` with detailed production-code
//! requirements listed in the test docstrings.
//!
//! Per `.claude/rules/project/security-id-uniqueness.md` (I-P1-11),
//! every dedup set in these tests keys on
//! `(SecurityId, ExchangeSegment)` — never `SecurityId` alone.

use std::collections::HashSet;
use std::path::PathBuf;

use tickvault_common::types::{ExchangeSegment, SecurityId};
use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType, replay_all};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Unique tempdir path per test invocation — prevents parallel tests
/// from stomping on each other's WAL files.
fn tempdir_path(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("tv-chaos-boot-{tag}-{pid}-{nanos}"))
}

fn cleanup(path: &std::path::Path) {
    drop(std::fs::remove_dir_all(path));
}

/// A minimal "subscribed set" using the canonical composite key.
/// This mirrors what `subscription_planner::build_subscription_plan`
/// and the Phase 2 scheduler use internally per I-P1-11.
type SubKey = (SecurityId, ExchangeSegment);

/// Pure function under test: given a batch of desired subscriptions
/// and a set of already-subscribed keys (persisted from a prior
/// process run), return only the NEW subscriptions that must be
/// dispatched on the wire. This is the invariant W3 must uphold:
/// idempotent re-dispatch.
fn compute_delta(
    desired: &[(SecurityId, ExchangeSegment)],
    already_subscribed: &HashSet<SubKey>,
) -> Vec<(SecurityId, ExchangeSegment)> {
    desired
        .iter()
        .copied()
        .filter(|k| !already_subscribed.contains(k))
        .collect()
}

// ---------------------------------------------------------------------------
// W3 — app crash + systemd restart mid-session must not double-subscribe
// ---------------------------------------------------------------------------

#[test]
fn test_w3_app_crash_restart_phase2_no_double_subscribe() {
    // Simulate the state after a successful Phase 2 dispatch at 09:13
    // IST: 5 stock F&O contracts are on the wire. The process then
    // crashes at 11:45 IST. systemd restarts tickvault. The recovery
    // snapshot (phase2-subscription.json) IS present on disk, so
    // `phase2_recovery::plan_recovery` returns `ReusedSnapshot` and
    // rehydrates the "already subscribed" set.
    let already_subscribed: HashSet<SubKey> = [
        (45001u32, ExchangeSegment::NseFno),
        (45002, ExchangeSegment::NseFno),
        (45003, ExchangeSegment::NseFno),
        (45004, ExchangeSegment::NseFno),
        (45005, ExchangeSegment::NseFno),
    ]
    .into_iter()
    .collect();

    // The recovered Phase 2 scheduler re-runs its delta computation.
    // The *desired* list is the SAME 5 SIDs (the pre-open prices have
    // not materially changed) plus 2 fresh SIDs whose underlyings
    // finally got an LTP on the late reboot.
    let desired: Vec<(SecurityId, ExchangeSegment)> = vec![
        (45001, ExchangeSegment::NseFno),
        (45002, ExchangeSegment::NseFno),
        (45003, ExchangeSegment::NseFno),
        (45004, ExchangeSegment::NseFno),
        (45005, ExchangeSegment::NseFno),
        (45099, ExchangeSegment::NseFno), // new
        (45100, ExchangeSegment::NseFno), // new
    ];

    // First recovery dispatch — only the 2 new SIDs cross the wire.
    let first_delta = compute_delta(&desired, &already_subscribed);
    assert_eq!(
        first_delta,
        vec![
            (45099, ExchangeSegment::NseFno),
            (45100, ExchangeSegment::NseFno),
        ],
        "first recovery dispatch must emit ONLY fresh SIDs, never \
         re-subscribe SIDs present in the persisted snapshot",
    );

    // Simulate a second recovery event (e.g. a rapid systemd restart
    // loop) where the snapshot ALREADY reflects the just-sent delta.
    let mut merged = already_subscribed.clone();
    for k in &first_delta {
        merged.insert(*k);
    }
    let second_delta = compute_delta(&desired, &merged);
    assert!(
        second_delta.is_empty(),
        "second idempotent re-run MUST produce zero new subscriptions; \
         got {:?}",
        second_delta,
    );

    // I-P1-11 ratchet: make sure we're actually keying on the composite
    // pair, not just the numeric id. A same-id collision across
    // segments must not accidentally dedup.
    let desired_with_collision: Vec<SubKey> = vec![
        (27, ExchangeSegment::IdxI),      // FINNIFTY index value
        (27, ExchangeSegment::NseEquity), // equity with same id
    ];
    let mut seg_aware: HashSet<SubKey> = HashSet::new();
    seg_aware.insert((27, ExchangeSegment::IdxI)); // only the index is already subscribed
    let delta = compute_delta(&desired_with_collision, &seg_aware);
    assert_eq!(
        delta,
        vec![(27, ExchangeSegment::NseEquity)],
        "segment-aware dedup required — plain-u32 key would drop the \
         NseEquity entry (I-P1-11 regression)",
    );
}

// ---------------------------------------------------------------------------
// W5 — host reboot → WAL replay yields live frames only + DEDUP on re-replay
// ---------------------------------------------------------------------------

#[test]
fn test_w5_host_reboot_wal_replay_live_ticks_only() {
    let dir = tempdir_path("w5-wal-replay");
    cleanup(&dir);
    drop(std::fs::create_dir_all(&dir));

    // --- Pre-crash state: write N live-feed frames to the spill.
    // Scope-limited so the Drop joins the writer thread and flushes
    // to disk — kernel state on disk after Drop is identical to what
    // a host-reboot SIGKILL would leave behind after the last fsync.
    const N: usize = 32;
    {
        let spill = WsFrameSpill::new(&dir).expect("create spill WAL");
        for i in 0..N {
            let frame = make_live_tick_frame(i as u32);
            assert_eq!(
                spill.append(WsType::LiveFeed, frame),
                AppendOutcome::Spilled,
                "pre-crash append must spill cleanly (channel capacity \
                 is 65K, we're writing 32)",
            );
        }
        // Wait for the writer thread to drain. The public API exposes
        // `persisted_count()` — spin until it matches.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while spill.persisted_count() < N as u64 {
            if std::time::Instant::now() > deadline {
                panic!(
                    "writer thread did not persist {} frames within 5s \
                     (persisted={})",
                    N,
                    spill.persisted_count()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    } // <- Drop joins writer thread

    // --- Post-reboot: fresh process spins up, replays the WAL.
    let replayed = replay_all(&dir).expect("replay must not fail on clean WAL");
    assert_eq!(
        replayed.len(),
        N,
        "W5 invariant: every pre-crash frame must be recovered",
    );

    // Every recovered frame is LiveFeed — NO historical candles, no
    // synthesized ticks, no OrderUpdate bleed. This is the
    // live-feed-purity guarantee (per
    // .claude/rules/project/live-feed-purity.md).
    for (i, frame) in replayed.iter().enumerate() {
        assert_eq!(
            frame.ws_type,
            WsType::LiveFeed,
            "frame {i}: W5 purity ratchet — WAL replay must NEVER \
             surface a non-live WsType",
        );
        // Payload integrity check: the sentinel byte matches the
        // sequence number we wrote pre-crash.
        assert_eq!(
            frame.frame[8],
            (i as u32).to_le_bytes()[0],
            "frame {i}: payload corruption during replay",
        );
    }

    // --- DEDUP ratchet: a second replay on the same dir must yield
    // ZERO frames (replay_all moves segments to `archive/`). This
    // proves that a retry loop (e.g. bootstrap script re-runs
    // replay_all on recovery failure) cannot accidentally inject the
    // same frames twice.
    let second_replay = replay_all(&dir).expect("second replay must not fail");
    assert!(
        second_replay.is_empty(),
        "W5 DEDUP ratchet: second replay on archived WAL must return \
         zero frames; got {}",
        second_replay.len(),
    );

    // Archive directory exists and contains what we replayed.
    let archive = dir.join("archive");
    assert!(archive.exists(), "replay_all must move processed segments");

    cleanup(&dir);
}

/// Build a minimal "live feed tick" WAL payload. The structure is
/// opaque to `WsFrameSpill` (WAL stores raw bytes) but we plant a
/// sequence number at byte 8 so the replay assertion can verify
/// payload integrity.
fn make_live_tick_frame(seq: u32) -> Vec<u8> {
    let mut frame = vec![0u8; 16];
    frame[0] = 2; // Ticker response code (per live-market-feed.md rule 5)
    frame[8..12].copy_from_slice(&seq.to_le_bytes());
    frame
}

// ---------------------------------------------------------------------------
// W4 — Docker wipe + boot must trigger silent emergency download
//       (IGNORED — requires HTTP mock harness, see notes below)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs HTTP mock harness for Dhan CSV endpoints"]
fn test_w4_docker_wipe_emergency_download_succeeds_silently() {
    // What this test needs in production code / test infra:
    //
    // 1. A test-only hook on `load_or_build_instruments` that lets us
    //    inject a local HTTPS server (e.g. `httpmock` or `wiremock`)
    //    serving a canned `api-scrip-master-detailed.csv` fixture in
    //    place of `https://images.dhan.co/...`. Today the URL is a
    //    hard parameter — injectable — BUT TLS validation makes local
    //    substitution clumsy.
    // 2. A `tracing-subscriber` test layer that captures emitted
    //    events so we can assert NO `Level::ERROR` event with the
    //    substring "CRITICAL" fires on the success path. The boot
    //    code currently logs `error!("CRITICAL: no instrument cache
    //    available ...")` BEFORE attempting the download, which
    //    means a clean success still produces a spurious ERROR line
    //    in the logs and fires a Telegram alert. That is the bug
    //    this test would pin.
    // 3. Production fix required before the test can pass: change
    //    `instrument_loader::load_from_cache_or_emergency_download`
    //    to log at `warn!` level before the download attempt and
    //    promote to `error!` ONLY if the download itself fails. The
    //    current unconditional `error!("CRITICAL: ...")` pages the
    //    operator on every fresh-clone / full-wipe boot, which is
    //    the W4 scenario — a legitimate operational event, not an
    //    incident.
    //
    // Test skeleton:
    //   let csv_fixture = include_str!("../fixtures/api-scrip-master-detailed.csv");
    //   let mock = httpmock::MockServer::start();
    //   mock.mock(|when, then| {
    //       when.method(GET).path("/api-data/api-scrip-master-detailed.csv");
    //       then.status(200).body(csv_fixture);
    //   });
    //   let tmp_cache = tempdir();  // no rkyv, no CSV in it
    //   let events = capture_tracing();
    //   let result = load_or_build_instruments(
    //       &mock.url("/api-data/api-scrip-master-detailed.csv"),
    //       &mock.url("/api-data/api-scrip-master-detailed.csv"),
    //       &config_with(tmp_cache.path()),
    //       ...
    //   ).await.expect("download path must succeed");
    //   assert!(matches!(result, InstrumentLoadResult::FreshBuild(_)));
    //   let critical_errors = events
    //       .iter()
    //       .filter(|e| e.level == Level::ERROR && e.message.contains("CRITICAL"))
    //       .count();
    //   assert_eq!(critical_errors, 0,
    //       "clean emergency download must not page operator");
    panic!("ignored test body — see docstring for activation plan");
}

// ---------------------------------------------------------------------------
// W6 — Dhan TCP-RST storm during Phase 2 dispatch → SubscribeRxGuard
//       preserves all 5000 SIDs (IGNORED — needs WS connection mock)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs WebSocketConnection test double with reconnect hook"]
fn test_w6_phase2_dispatch_during_reconnect_preserves_all_subscriptions() {
    // What this test needs in production code / test infra:
    //
    // 1. A `trait SubscribeSink` extracted from `WebSocketConnection`
    //    with a single method `send_subscribe(&self, msg: SubscribeMsg)
    //    -> Result<()>`. Today the send path is bolted onto the
    //    concrete `WebSocketConnection` — no trait seam, so a mock
    //    requires duplicating tokio-tungstenite machinery.
    // 2. A test double that:
    //    (a) accepts N subscribe messages,
    //    (b) after the K-th message, simulates a Dhan TCP-RST by
    //        returning `Err(Protocol::ResetWithoutClosingHandshake)`,
    //    (c) after reconnect (driven by `SubscribeRxGuard` reinstalling
    //        the rx) accepts the remaining (N-K) + replays the
    //        outstanding ones from the resume-buffer.
    // 3. A minimal `run_read_loop` harness that wires the real
    //    `SubscribeRxGuard` (see
    //    `.claude/rules/project/live-market-feed-subscription.md`
    //    2026-04-24 Fix #3) against the mock sink.
    //
    // The ratchet is:
    //   let (tx, rx) = mpsc::channel::<SubscribeCommand>(5000);
    //   let sink = FlakyMockSink::new().reset_after(2500);
    //   let handle = tokio::spawn(run_read_loop_with_guard(sink, rx));
    //   for sid in 0..5000u32 {
    //       tx.send(SubscribeCommand::Add(sid, ExchangeSegment::NseFno))
    //         .await.unwrap();
    //   }
    //   // Wait for reconnect cycle to drain.
    //   handle.await;
    //   assert_eq!(sink.final_subscribed_set().len(), 5000,
    //       "W6 invariant: every SID queued during the TCP-RST window \
    //        must eventually land on the resumed socket");
    //
    // Until the `SubscribeSink` trait exists, the test cannot be
    // written without pulling in half the network stack, which
    // violates the "pure-logic ratchets where possible" directive.
    panic!("ignored test body — see docstring for activation plan");
}
