//! CASCADE-01 — triple-failure recovery chaos test (Wave-4-E3).
//!
//! Proves the zero-durable-loss invariant holds when THREE independent
//! failures strike simultaneously inside one ~60-second window:
//!
//!   1. **WebSocket drop** — the downstream tick consumer is gone, yet the
//!      WS read loop keeps `append`ing every raw frame to the on-disk WAL
//!      (`WsFrameSpill`). Each append MUST return `Spilled` (never `Dropped`),
//!      and `replay_all` MUST recover every frame, in order, on the next boot.
//!
//!   2. **QuestDB down** — RETIRED (stage-2 dead-WS sweep, 2026-07-17):
//!      this leg exercised `TickPersistenceWriter`'s ring → disk-spill → DLQ
//!      absorption, deleted with the dead Dhan tick chain (no live `ticks`
//!      writer remains; runtime is REST-only). The surviving QuestDB-outage
//!      absorption chain is the SEAL writer's ring→spill→DLQ, chaos-covered
//!      by `crates/storage/tests/chaos_rescue_ring_overflow.rs` +
//!      `chaos_questdb_docker_pause.rs`.
//!
//!   3. **Auth token expiry** — the JWT is past its expiry instant. The
//!      recovery primitives that drive `force_renewal_if_stale` MUST signal
//!      "engage renewal" (`TokenState::is_valid() == false`,
//!      `needs_refresh() == true`).
//!      (PR-C2 trim, 2026-07-13: the main-feed reconnect-backoff ladder
//!      assertions retired with `connection.rs::compute_reconnect_base_delay_ms`
//!      — deleted with the Dhan live-WS lane; the surviving order-update
//!      backoff ladder is unit-tested in `order_update_connection.rs`.)
//!
//! The unifying, asserted invariant across the surviving legs is **ZERO
//! durable loss**: every captured frame survives the WAL belt, and each
//! recovery primitive engages.
//!
//! This test lives in `crates/core/tests/` (not `crates/storage/`) because the
//! token recovery primitives are owned by `tickvault-core`, while
//! `tickvault-storage` (the WAL) is a `tickvault-core` dependency — so the
//! surviving legs are reachable here and every one is a REAL public-API call
//! (no source-scan stand-ins).
//!
//! NON-Docker, deterministic, bounded waits only — runs in well under 5 s on a
//! cold laptop. A `#[ignore]` Docker-backed variant mirrors `chaos_zero_tick_loss.rs`
//! for a live cascade and is skipped by default.

#![cfg(test)]
// Test convention (matches every other chaos test file): unwrap/expect are
// fine in test code — the harness reports the panic with a clear message.
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tickvault_core::auth::types::{DhanAuthResponseData, TokenState};
use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType, replay_all};

/// Number of frames pushed through the WAL belt within the
/// simultaneous-failure window.
const N: usize = 5_000;

/// A unique temp dir per tag + process, removed first so a stale run can't
/// pollute the assertion. Matches `chaos_ws_frame_spill_saturation::chaos_tmp`.
fn chaos_tmp(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!(
        "tv-cascade-triple-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create cascade temp dir");
    p
}

/// Bounded poll on `persisted_count()` — never an unbounded sleep. Returns the
/// observed count (which may be `< target` if the deadline expired).
fn wait_until_wal_persisted(spill: &WsFrameSpill, target: u64, budget: Duration) -> u64 {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        let count = spill.persisted_count();
        if count >= target {
            return count;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    spill.persisted_count()
}

/// Build a token that expired `secs_ago` seconds ago (negative `expires_in`
/// puts `expires_at` in the past). This is a REAL `TokenState` exercised
/// through its public predicates — the same predicates whose decision drives
/// `TokenManager::force_renewal_if_stale`.
fn expired_token() -> TokenState {
    // expires_in = 0 → expires_at == issued_at == now; by the time the
    // predicates run a moment later, now > expires_at → expired. Use a clearly
    // negative offset for determinism regardless of scheduling jitter.
    // `expires_in` is u32, so the earliest token we can mint expires "now"
    // (validity = 0). A bounded 50 ms sleep then puts the wall clock
    // unambiguously past `expires_at` — deterministic, no unbounded wait.
    let response = DhanAuthResponseData {
        access_token: "eyJexpired".to_string(),
        token_type: "Bearer".to_string(),
        expires_in: 0,
    };
    let state = TokenState::from_response(&response);
    std::thread::sleep(Duration::from_millis(50));
    state
}

// ---------------------------------------------------------------------------
// CASCADE-01 — the deterministic, always-run test.
// ---------------------------------------------------------------------------

/// All three failures injected at once; the single asserted invariant is
/// ZERO durable loss across every belt, plus engagement of each recovery
/// primitive.
#[test]
fn cascade_01_triple_failure_loses_zero_durable_data() {
    let wal_dir = chaos_tmp("wal");

    // ---- LEG 3 (set up first; it gates the live window) -------------------
    // "Auth token expired": a real expired TokenState + the reconnect-backoff
    // helper. The recovery decision (renew? back off?) is what we assert.
    let token = expired_token();
    assert!(
        !token.is_valid(),
        "LEG 3 setup: token must be EXPIRED so the renewal primitive engages"
    );
    // `force_renewal_if_stale(14_400)` renews when remaining <= 4h; an expired
    // token is the strongest case (remaining < 0). `needs_refresh` is the pure
    // predicate that mirrors that decision.
    assert!(
        token.needs_refresh(4),
        "LEG 3: an expired token MUST signal 'engage renewal' (needs_refresh)"
    );

    // ---- LEG 2: "QuestDB down" — RETIRED (stage-2 dead-WS sweep,
    // 2026-07-17): the `TickPersistenceWriter` belt was deleted with the
    // dead Dhan tick chain; the seal chain's QuestDB-outage absorption is
    // chaos-covered in crates/storage/tests/. ---------------------------

    // ---- LEG 1: "WebSocket drop" -----------------------------------------
    // The downstream consumer is gone, but the WS read loop keeps capturing
    // every raw frame into the durable WAL.
    let spill = WsFrameSpill::new(&wal_dir).expect("WsFrameSpill::new");

    // ---- The simultaneous-failure window ---------------------------------
    // Feed N frames through the WAL belt while the failures are active.
    let mut wal_dropped = 0u64;
    for i in 0..N {
        // LEG 1 — capture the raw frame (16-byte ticker-sized payload, first 4
        // bytes = sequence so replay order is auditable).
        let mut frame = vec![0u8; 16];
        frame[0..4].copy_from_slice(&(i as u32).to_le_bytes());
        if spill.append(WsType::LiveFeed, frame) == AppendOutcome::Dropped {
            wal_dropped += 1;
        }
    }

    // ======================================================================
    // INVARIANT 1 — LEG 1: zero WS-frame loss; WAL replay recovers all N in
    // order. Proves the "WebSocket drop" did not lose a single captured frame.
    // ======================================================================
    assert_eq!(
        wal_dropped, 0,
        "LEG 1: WAL must absorb every captured frame during the cascade \
         (got {wal_dropped} drops of {N})"
    );
    assert_eq!(
        spill.drop_critical_count(),
        0,
        "LEG 1: tv_ws_frame_spill_drop_critical must stay at zero — any drop \
         is durable WS-frame loss"
    );
    // Let the writer thread fully persist, then drop so it drains + exits and
    // replay sees archived segments cleanly.
    let persisted = wait_until_wal_persisted(&spill, N as u64, Duration::from_secs(10));
    assert_eq!(
        persisted, N as u64,
        "LEG 1: WAL writer must persist every frame ({persisted}/{N})"
    );
    drop(spill);
    // Bounded settle for the writer thread to flush + exit before replay.
    std::thread::sleep(Duration::from_millis(50));

    let recovered = replay_all(&wal_dir).expect("replay_all");
    assert_eq!(
        recovered.len(),
        N,
        "LEG 1: WAL replay must recover every captured frame after the WS drop"
    );
    for (i, rec) in recovered.iter().enumerate() {
        assert_eq!(
            rec.ws_type,
            WsType::LiveFeed,
            "LEG 1: recovered frame {i} has the wrong WsType tag"
        );
        let seq = u32::from_le_bytes([rec.frame[0], rec.frame[1], rec.frame[2], rec.frame[3]]);
        assert_eq!(
            seq, i as u32,
            "LEG 1: WAL replay must preserve FIFO order — frame {i} out of order"
        );
    }

    // INVARIANT 2 — LEG 2: RETIRED (stage-2 dead-WS sweep, 2026-07-17) —
    // the `ticks_dropped_total()/dlq_ticks_total()/buffered_tick_count()`
    // accounting died with the deleted TickPersistenceWriter.

    // ======================================================================
    // INVARIANT 3 — LEG 3: the auth-recovery primitives engage. The expired
    // token drives renewal. (PR-C2, 2026-07-13: the main-feed
    // reconnect-backoff ladder assertions retired with the deleted
    // `connection.rs` — the order-update ladder keeps its own unit suite.)
    // ======================================================================
    // Re-assert the token is still expired post-window (no clock surprise).
    assert!(
        !token.is_valid() && token.needs_refresh(4),
        "LEG 3: expired token must consistently signal renewal-engagement"
    );

    // ---- Cleanup ----------------------------------------------------------
    let _ = std::fs::remove_dir_all(&wal_dir);
}

/// Guard: the three legs are independent — exercising the WAL + tick belts in
/// the SAME window does not let one belt's failure mask the other's success.
/// Re-runs the two durable belts in isolation and confirms identical
/// zero-loss accounting, so a future refactor that accidentally couples them
/// (e.g. a shared global spill path) fails here.
#[test]
fn cascade_01_legs_are_independent_zero_loss_each() {
    // WAL belt alone.
    let wal_dir = chaos_tmp("indep-wal");
    {
        let spill = WsFrameSpill::new(&wal_dir).expect("spill new");
        for i in 0..1_000u32 {
            assert_eq!(
                spill.append(WsType::OrderUpdate, i.to_le_bytes().to_vec()),
                AppendOutcome::Spilled,
                "WAL belt must spill every frame in isolation"
            );
        }
        let got = wait_until_wal_persisted(&spill, 1_000, Duration::from_secs(5));
        assert_eq!(got, 1_000, "WAL belt must persist all 1,000 frames");
        assert_eq!(spill.drop_critical_count(), 0);
    }
    let recovered = replay_all(&wal_dir).expect("replay");
    assert_eq!(recovered.len(), 1_000, "WAL belt replay must recover all");

    // Tick belt alone — RETIRED (stage-2 dead-WS sweep, 2026-07-17): the
    // TickPersistenceWriter belt was deleted with the dead Dhan tick chain.

    let _ = std::fs::remove_dir_all(&wal_dir);
}

// ---------------------------------------------------------------------------
// Docker-backed live cascade variant — mirrors chaos_zero_tick_loss.rs.
// #[ignore] by default; skips cleanly when the stack isn't running.
// ---------------------------------------------------------------------------

const DOCKER_COMPOSE_PATH: &str = "deploy/docker/docker-compose.yml";
const QUESTDB_SERVICE: &str = "tv-questdb";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn docker_stack_available() -> bool {
    let out = std::process::Command::new("docker")
        .current_dir(workspace_root())
        .args([
            "compose",
            "-f",
            DOCKER_COMPOSE_PATH,
            "ps",
            "--services",
            "--filter",
            "status=running",
        ])
        .output();
    match out {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            stdout.lines().any(|l| l.trim() == QUESTDB_SERVICE)
        }
        Err(_) => false,
    }
}

fn pause_questdb() -> std::io::Result<()> {
    let status = std::process::Command::new("docker")
        .current_dir(workspace_root())
        .args([
            "compose",
            "-f",
            DOCKER_COMPOSE_PATH,
            "pause",
            QUESTDB_SERVICE,
        ])
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "docker compose pause returned {status}"
        )));
    }
    Ok(())
}

fn unpause_questdb() -> std::io::Result<()> {
    let status = std::process::Command::new("docker")
        .current_dir(workspace_root())
        .args([
            "compose",
            "-f",
            DOCKER_COMPOSE_PATH,
            "unpause",
            QUESTDB_SERVICE,
        ])
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "docker compose unpause returned {status}"
        )));
    }
    Ok(())
}

/// Live cascade: pause QuestDB (real outage) while the WAL belt captures frames
/// and the expired-token primitive signals renewal, then unpause and confirm
/// the WAL still replays every frame. Asserts zero durable WS-frame loss across
/// a REAL QuestDB outage rather than a dead-port simulation.
#[test]
#[ignore = "requires Docker compose stack — run with `cargo test -- --ignored`"]
fn cascade_01_triple_failure_live_docker_zero_loss() {
    if !docker_stack_available() {
        eprintln!("SKIP: docker compose stack not running (run `make docker-up` first)");
        return;
    }

    let wal_dir = chaos_tmp("live-wal");
    let spill = WsFrameSpill::new(&wal_dir).expect("spill new");

    // LEG 3 — expired token engages renewal during the live outage.
    let token = expired_token();
    assert!(!token.is_valid() && token.needs_refresh(4));

    // LEG 2 — real QuestDB outage.
    pause_questdb().expect("docker pause");

    // LEG 1 — capture frames into the WAL while QuestDB is down.
    let live_n = 2_000u32;
    for i in 0..live_n {
        let mut frame = vec![0u8; 16];
        frame[0..4].copy_from_slice(&i.to_le_bytes());
        assert_eq!(
            spill.append(WsType::LiveFeed, frame),
            AppendOutcome::Spilled,
            "WAL must capture frame {i} during the live QuestDB outage"
        );
    }
    let _ = wait_until_wal_persisted(&spill, u64::from(live_n), Duration::from_secs(10));

    unpause_questdb().expect("docker unpause");

    assert_eq!(
        spill.drop_critical_count(),
        0,
        "LEG 1 (live): zero WS-frame loss across a real QuestDB outage"
    );
    drop(spill);
    std::thread::sleep(Duration::from_millis(50));
    let recovered = replay_all(&wal_dir).expect("replay");
    assert_eq!(
        recovered.len(),
        live_n as usize,
        "LEG 1 (live): WAL replay must recover every frame"
    );

    let _ = std::fs::remove_dir_all(&wal_dir);
}
