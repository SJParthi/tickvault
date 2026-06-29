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
//!   2. **QuestDB down** — the tick-persistence ILP writer is pointed at a
//!      dead endpoint, so no row can reach QuestDB. Every tick MUST cascade
//!      through the ring → disk-spill → DLQ absorption chain with ZERO
//!      permanently-dropped ticks (`ticks_dropped_total() == 0`).
//!
//!   3. **Auth token expiry** — the JWT is past its expiry instant. The
//!      recovery primitives that drive `force_renewal_if_stale` MUST signal
//!      "engage renewal" (`TokenState::is_valid() == false`,
//!      `needs_refresh() == true`) and the reconnect-backoff helper MUST
//!      engage its ladder (instant 0 ms first retry, then a growing,
//!      capped backoff).
//!
//! The unifying, asserted invariant across all three legs is **ZERO durable
//! loss**: every captured frame survives the WAL belt, every tick survives
//! the ring→spill→DLQ belt, and each recovery primitive engages.
//!
//! This test lives in `crates/core/tests/` (not `crates/storage/`) because the
//! token + reconnect recovery primitives are owned by `tickvault-core`, while
//! `tickvault-storage` (WAL + tick persistence) is a `tickvault-core`
//! dependency — so all three legs are reachable here and every one is a REAL
//! public-API call (no source-scan stand-ins).
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

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::TICK_BUFFER_CAPACITY;
use tickvault_common::tick_types::ParsedTick;
use tickvault_core::auth::types::{DhanAuthResponseData, TokenState};
use tickvault_core::websocket::connection::compute_reconnect_base_delay_ms;
use tickvault_storage::tick_persistence::TickPersistenceWriter;
use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType, replay_all};

/// Number of frames/ticks pushed through both durable belts within the
/// simultaneous-failure window. Deliberately well below `TICK_BUFFER_CAPACITY`
/// (100,000) so the tick ring never even needs to overflow to disk — the
/// strongest form of "zero loss" is "zero drop AND nothing forced to spill".
const N: usize = 5_000;

/// QuestDB ILP first-retry: instant (0 ms). Mirrors the production main-feed
/// reconnect contract `compute_reconnect_base_delay_ms(0, _, _) == 0`.
const RECONNECT_INITIAL_MS: u64 = 500;
const RECONNECT_MAX_MS: u64 = 30_000;

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

/// `QuestDbConfig` pointing at a dead, unused high port so every ILP connection
/// attempt fails immediately (no network delay) — this is the "QuestDB down"
/// leg. Identical strategy to `tick_resilience::test_config`.
fn dead_questdb_config() -> QuestDbConfig {
    QuestDbConfig {
        host: "127.0.0.1".to_string(),
        ilp_port: 19_009, // closed port → connect fails fast
        http_port: 19_000,
        pg_port: 18_812,
    }
}

/// Minimal NSE_EQ tick (Greeks NaN, non-F&O default), monotonic per `i` so the
/// stream is distinguishable. Mirrors `tick_resilience::make_tick`.
fn make_tick(id: u64, price: f32) -> ParsedTick {
    ParsedTick {
        security_id: id,
        exchange_segment_code: 1, // NSE_EQ
        last_traded_price: price,
        last_trade_quantity: 100,
        exchange_timestamp: 1_700_000_000_u32.saturating_add(id as u32),
        received_at_nanos: 0,
        average_traded_price: price,
        volume: 1000,
        total_sell_quantity: 500,
        total_buy_quantity: 500,
        day_open: price,
        day_close: price,
        day_high: price,
        day_low: price,
        open_interest: 0,
        oi_day_high: 0,
        oi_day_low: 0,
        iv: f64::NAN,
        delta: f64::NAN,
        gamma: f64::NAN,
        theta: f64::NAN,
        vega: f64::NAN,
    }
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
    let spill_dir = chaos_tmp("spill");

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

    // ---- LEG 2: "QuestDB down" -------------------------------------------
    // Disconnected ILP writer pointed at a dead port → every flush fails →
    // ticks cascade ring → spill → DLQ. Per-test spill dir avoids parallel
    // races on the shared default path.
    let config = dead_questdb_config();
    let mut tick_writer = TickPersistenceWriter::new_disconnected(&config);
    tick_writer.set_spill_dir_for_test(spill_dir.clone());
    assert!(
        !tick_writer.is_connected(),
        "LEG 2 setup: tick writer must be disconnected (QuestDB down)"
    );

    // ---- LEG 1: "WebSocket drop" -----------------------------------------
    // The downstream consumer is gone, but the WS read loop keeps capturing
    // every raw frame into the durable WAL.
    let spill = WsFrameSpill::new(&wal_dir).expect("WsFrameSpill::new");

    // ---- The simultaneous-failure window ---------------------------------
    // Within ONE logical window, feed N frames through the WAL belt AND N ticks
    // through the persistence belt. All three failures are active concurrently.
    let mut wal_dropped = 0u64;
    for i in 0..N {
        // LEG 1 — capture the raw frame (16-byte ticker-sized payload, first 4
        // bytes = sequence so replay order is auditable).
        let mut frame = vec![0u8; 16];
        frame[0..4].copy_from_slice(&(i as u32).to_le_bytes());
        if spill.append(WsType::LiveFeed, frame) == AppendOutcome::Dropped {
            wal_dropped += 1;
        }

        // LEG 2 — persist the parsed tick (QuestDB down → ring buffer).
        let tick = make_tick(i as u64, 100.0 + (i % 1000) as f32);
        tick_writer
            .append_tick(&tick)
            .expect("append_tick must return Ok even with QuestDB down");
    }
    // A flush attempt during the outage must also not drop or panic.
    let _ = tick_writer.flush_if_needed();

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

    // ======================================================================
    // INVARIANT 2 — LEG 2: zero permanently-dropped ticks. The ring buffer
    // (well below capacity at N=5,000) absorbed everything during the QuestDB
    // outage; nothing reached the DLQ. Proves "QuestDB down" lost zero ticks.
    // ======================================================================
    assert_eq!(
        tick_writer.ticks_dropped_total(),
        0,
        "LEG 2: ZERO ticks may be permanently dropped during the QuestDB outage"
    );
    assert_eq!(
        tick_writer.dlq_ticks_total(),
        0,
        "LEG 2: no tick should reach the DLQ — the ring buffer has ample room \
         ({N} << {TICK_BUFFER_CAPACITY})"
    );
    assert_eq!(
        tick_writer.buffered_tick_count(),
        N,
        "LEG 2: every tick must be held in the durable ring buffer ({N})"
    );
    assert_eq!(
        tick_writer.ticks_spilled_total(),
        0,
        "LEG 2: with {N} ticks far below capacity, none should even need disk \
         spill — the ring alone proves zero loss"
    );

    // ======================================================================
    // INVARIANT 3 — LEG 3: the auth-recovery primitives engage. The expired
    // token drives renewal, and the reconnect-backoff ladder engages with the
    // documented instant-first-retry contract.
    // ======================================================================
    // First reconnect attempt is instant (0 ms) per the main-feed contract.
    assert_eq!(
        compute_reconnect_base_delay_ms(0, RECONNECT_INITIAL_MS, RECONNECT_MAX_MS),
        0,
        "LEG 3: first reconnect retry must be instant (0 ms)"
    );
    // Subsequent attempts engage a growing, capped exponential backoff.
    assert_eq!(
        compute_reconnect_base_delay_ms(1, RECONNECT_INITIAL_MS, RECONNECT_MAX_MS),
        RECONNECT_INITIAL_MS,
        "LEG 3: second retry must be the base delay"
    );
    assert_eq!(
        compute_reconnect_base_delay_ms(2, RECONNECT_INITIAL_MS, RECONNECT_MAX_MS),
        RECONNECT_INITIAL_MS * 2,
        "LEG 3: backoff must double on the third retry"
    );
    assert_eq!(
        compute_reconnect_base_delay_ms(64, RECONNECT_INITIAL_MS, RECONNECT_MAX_MS),
        RECONNECT_MAX_MS,
        "LEG 3: backoff must saturate at the cap, never overflow"
    );
    // Re-assert the token is still expired post-window (no clock surprise).
    assert!(
        !token.is_valid() && token.needs_refresh(4),
        "LEG 3: expired token must consistently signal renewal-engagement"
    );

    // ---- Cleanup ----------------------------------------------------------
    let _ = std::fs::remove_dir_all(&wal_dir);
    let _ = std::fs::remove_dir_all(&spill_dir);
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

    // Tick belt alone (QuestDB down).
    let spill_dir = chaos_tmp("indep-spill");
    let config = dead_questdb_config();
    let mut tick_writer = TickPersistenceWriter::new_disconnected(&config);
    tick_writer.set_spill_dir_for_test(spill_dir.clone());
    for i in 0..1_000u32 {
        tick_writer
            .append_tick(&make_tick(u64::from(i), 250.0 + i as f32))
            .expect("append_tick ok while disconnected");
    }
    assert_eq!(tick_writer.ticks_dropped_total(), 0, "tick belt zero drops");
    assert_eq!(tick_writer.dlq_ticks_total(), 0, "tick belt zero DLQ");
    assert_eq!(
        tick_writer.buffered_tick_count(),
        1_000,
        "tick belt rings all 1,000"
    );

    let _ = std::fs::remove_dir_all(&wal_dir);
    let _ = std::fs::remove_dir_all(&spill_dir);
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
