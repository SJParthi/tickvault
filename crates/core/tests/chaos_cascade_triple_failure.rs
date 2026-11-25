//! CASCADE-01 — the three failures of the "triple failure" scenario, and where
//! each one is really tested (Wave-4-E3; corrected 2026-11-07).
//!
//!   1. **WebSocket drop** — the downstream tick consumer is gone, yet the
//!      WS read loop keeps `append`ing every raw frame to the on-disk WAL
//!      (`WsFrameSpill`). Each append MUST return `Spilled` (never `Dropped`),
//!      and `replay_all` MUST recover every frame, in order, on the next boot.
//!      Tested HERE by
//!      `cascade_01_wal_absorbs_and_replays_every_frame_with_the_consumer_gone`.
//!
//!   2. **QuestDB down** — tested in the storage crate, by
//!      `chaos_cascade_01_leg2_questdb_down_offloaded_tick_rows_spill_then_land_once`
//!      in `crates/storage/src/tick_questdb_down_chaos_tests.rs`. It drives the
//!      LIVE tick writer (`TickWriter`, split for offload as the lane splits
//!      it, both row encoders) against an unreachable QuestDB (a closed local
//!      port, no Docker), asserts every row is rescued to the spill files,
//!      then replays them to an endpoint that came back and asserts each row
//!      lands exactly once. It lives there because it needs two seams private
//!      to that crate's tests: the writer's spill-directory override and the
//!      replay's test view of the sink.
//!      *(Corrected 2026-11-07: this paragraph said the leg was retired
//!      because "no live `ticks` writer remains; runtime is REST-only", and
//!      named a `chaos_questdb_docker_pause.rs` that does not exist. The tick
//!      writer returned with the 2026-08-09 Dhan live-feed revival, so the leg
//!      was rebuilt against it.)*
//!
//!   3. **Auth token expiry** — tested HERE only as far as the token's own
//!      predicates: `cascade_01_expired_token_state_reports_invalid_and_needs_refresh`
//!      checks that an expired `TokenState` reports `is_valid() == false` and
//!      `needs_refresh() == true`. No renewal code runs in this file. The
//!      renewal decision itself (`TokenManager::force_renewal_if_stale`) is
//!      unit-tested in `crates/core/src/auth/token_manager.rs`
//!      (`test_force_renewal_if_stale_*`), because its test constructor is
//!      private to the crate.
//!      *(Corrected 2026-11-07: this check used to sit inside a test named
//!      for the whole cascade, worded as if the renewal itself engaged.)*
//!      (PR-C2 trim, 2026-07-13: the main-feed reconnect-backoff ladder
//!      assertions retired with `connection.rs::compute_reconnect_base_delay_ms`
//!      — deleted with the Dhan live-WS lane; the surviving order-update
//!      backoff ladder is unit-tested in `order_update_connection.rs`.)
//!
//! NON-Docker, deterministic, bounded waits only — runs in well under 5 s on a
//! cold laptop. One `#[ignore]` Docker-backed test pauses the real QuestDB
//! container and checks that WAL capture does not depend on it; with
//! `TV_QDB_REAL_REQUIRED=1` a missing stack FAILS it instead of skipping.

#![cfg(test)]
// Test convention (matches every other chaos test file): unwrap/expect are
// fine in test code — the harness reports the panic with a clear message.
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tickvault_core::auth::types::{DhanAuthResponseData, TokenState};
use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType, replay_all};

/// Number of frames pushed through the WAL belt with the consumer gone.
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

/// Build a token that has already expired. `expires_in` is u32, so the
/// earliest token we can mint expires "now" (validity = 0). A bounded 50 ms
/// sleep then puts the wall clock unambiguously past `expires_at` —
/// deterministic, no unbounded wait.
fn expired_token() -> TokenState {
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
// CASCADE-01 LEG 1 — WebSocket drop: the WAL keeps every frame.
// ---------------------------------------------------------------------------

/// The consumer is gone, the WS read loop keeps capturing: every frame is
/// spilled, persisted and replayed in order.
#[test]
fn cascade_01_wal_absorbs_and_replays_every_frame_with_the_consumer_gone() {
    let wal_dir = chaos_tmp("wal");
    let spill = WsFrameSpill::new(&wal_dir).expect("WsFrameSpill::new");

    let mut wal_dropped = 0u64;
    for i in 0..N {
        // 16-byte ticker-sized payload, first 4 bytes = sequence so replay
        // order is auditable.
        let mut frame = vec![0u8; 16];
        frame[0..4].copy_from_slice(&(i as u32).to_le_bytes());
        if spill.append(WsType::LiveFeed, frame) == AppendOutcome::Dropped {
            wal_dropped += 1;
        }
    }

    assert_eq!(
        wal_dropped, 0,
        "LEG 1: WAL must absorb every captured frame (got {wal_dropped} drops of {N})"
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

    let _ = std::fs::remove_dir_all(&wal_dir);
}

/// The WAL alone, with order-update frames: every append spills, every frame
/// persists and replays. (Until 2026-11-07 this test claimed to check two
/// belts "in isolation"; the second belt, the deleted tick writer, had been
/// removed on 2026-07-17, so it only ever checked this one.)
#[test]
fn cascade_01_wal_alone_spills_and_replays_every_order_update_frame() {
    let wal_dir = chaos_tmp("indep-wal");
    {
        let spill = WsFrameSpill::new(&wal_dir).expect("spill new");
        for i in 0..1_000u32 {
            assert_eq!(
                spill.append(WsType::OrderUpdate, i.to_le_bytes().to_vec()),
                AppendOutcome::Spilled,
                "WAL must spill every frame"
            );
        }
        let got = wait_until_wal_persisted(&spill, 1_000, Duration::from_secs(5));
        assert_eq!(got, 1_000, "WAL must persist all 1,000 frames");
        assert_eq!(spill.drop_critical_count(), 0);
    }
    let recovered = replay_all(&wal_dir).expect("replay");
    assert_eq!(recovered.len(), 1_000, "WAL replay must recover all");
    assert!(
        recovered
            .iter()
            .all(|rec| rec.ws_type == WsType::OrderUpdate),
        "every replayed frame keeps its OrderUpdate tag"
    );

    let _ = std::fs::remove_dir_all(&wal_dir);
}

// ---------------------------------------------------------------------------
// CASCADE-01 LEG 3 — auth token expiry: the token's own predicates.
// ---------------------------------------------------------------------------

/// An expired token reports invalid and asks for a refresh. These are the
/// predicates only; the renewal decision is unit-tested in `token_manager.rs`
/// (see the module header).
#[test]
fn cascade_01_expired_token_state_reports_invalid_and_needs_refresh() {
    let token = expired_token();
    assert!(!token.is_valid(), "an expired token must not read valid");
    assert!(
        token.needs_refresh(4),
        "an expired token must read as inside the 4-hour refresh window"
    );
    // Still expired a moment later: no clock surprise flips it back.
    std::thread::sleep(Duration::from_millis(5));
    assert!(!token.is_valid() && token.needs_refresh(4));
}

// ---------------------------------------------------------------------------
// Docker-backed: WAL capture does not depend on a running QuestDB.
// #[ignore] by default. Without the stack it skips, unless
// TV_QDB_REAL_REQUIRED=1, which turns a missing stack into a failure.
// ---------------------------------------------------------------------------

const DOCKER_COMPOSE_PATH: &str = "deploy/docker/docker-compose.yml";
const QUESTDB_SERVICE: &str = "tv-questdb";
/// How long a probe waits for QuestDB's HTTP answer before calling it down.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long QuestDB may take to answer again after the unpause.
const RECOVERY_BUDGET: Duration = Duration::from_secs(30);

fn real_questdb_required() -> bool {
    std::env::var("TV_QDB_REAL_REQUIRED").is_ok_and(|v| v == "1")
}

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

fn compose(action: &str) -> std::io::Result<()> {
    let status = std::process::Command::new("docker")
        .current_dir(workspace_root())
        .args([
            "compose",
            "-f",
            DOCKER_COMPOSE_PATH,
            action,
            QUESTDB_SERVICE,
        ])
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "docker compose {action} returned {status}"
        )));
    }
    Ok(())
}

/// True when QuestDB's HTTP endpoint at `addr` answers within
/// [`PROBE_TIMEOUT`]. A paused container still completes the TCP handshake
/// (the kernel does it), so the probe waits for an HTTP status line, not for
/// the connect.
fn questdb_answers(addr: &str) -> bool {
    let Ok(target) = addr.parse::<std::net::SocketAddr>() else {
        panic!("TV_QDB_REAL_HTTP must be ip:port, got {addr}");
    };
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&target, PROBE_TIMEOUT) else {
        return false;
    };
    if stream.set_read_timeout(Some(PROBE_TIMEOUT)).is_err()
        || stream
            .write_all(b"GET /exec?query=select%201 HTTP/1.0\r\n\r\n")
            .is_err()
    {
        return false;
    }
    let mut head = [0u8; 8];
    stream.read_exact(&mut head).is_ok() && head.starts_with(b"HTTP/1.")
}

/// The probe the live test relies on to prove the outage is real: a listener
/// that accepts the connection and never answers (what a paused container
/// looks like) reads DOWN, a closed port reads DOWN, and a server that sends
/// an HTTP status line reads UP. Runs without Docker.
#[test]
fn cascade_01_questdb_probe_tells_a_frozen_listener_from_an_answering_one() {
    // Frozen: accepts, holds the connection, never writes.
    let frozen = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let frozen_addr = frozen.local_addr().expect("addr").to_string();
    let holder = std::thread::spawn(move || {
        let held = frozen.accept().map(|(stream, _)| stream);
        std::thread::sleep(PROBE_TIMEOUT + Duration::from_millis(500));
        drop(held);
    });
    assert!(
        !questdb_answers(&frozen_addr),
        "a listener that never answers must read as down"
    );
    let _ = holder.join();

    // Answering: replies with a status line.
    let answering = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let answering_addr = answering.local_addr().expect("addr").to_string();
    let server = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = answering.accept() {
            let mut request = [0u8; 64];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        }
    });
    assert!(
        questdb_answers(&answering_addr),
        "a server that sends an HTTP status line must read as up"
    );
    let _ = server.join();

    // Closed: nothing listens.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let closed_addr = closed.local_addr().expect("addr").to_string();
    drop(closed);
    assert!(
        !questdb_answers(&closed_addr),
        "a refused connection must read as down"
    );
}

/// Live: pause the real QuestDB container, prove it stopped answering, capture
/// frames into the WAL during the outage, unpause, prove it answers again,
/// and confirm the WAL replays every frame. What this shows is that WAL
/// capture never waits on QuestDB; the tick writer's rescue during an outage
/// is the storage-crate test named in the module header.
#[test]
#[ignore = "requires Docker compose stack — run with `cargo test -- --ignored`"]
fn cascade_01_wal_capture_survives_a_real_questdb_pause_live_docker() {
    let required = real_questdb_required();
    if !docker_stack_available() {
        assert!(
            !required,
            "TV_QDB_REAL_REQUIRED=1 but the docker compose stack is not running \
             ({QUESTDB_SERVICE} not listed by `docker compose ps`): this live test \
             would skip and still pass"
        );
        eprintln!("SKIP: docker compose stack not running (run `make docker-up` first)");
        return;
    }
    let Some(addr) = std::env::var("TV_QDB_REAL_HTTP")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        assert!(
            !required,
            "TV_QDB_REAL_REQUIRED=1 but TV_QDB_REAL_HTTP is unset: the test cannot \
             prove the pause took QuestDB down"
        );
        eprintln!("SKIP: set TV_QDB_REAL_HTTP=ip:port to prove the outage is real");
        return;
    };
    assert!(
        questdb_answers(&addr),
        "QuestDB at {addr} must answer before the pause, or the outage below proves nothing"
    );

    let wal_dir = chaos_tmp("live-wal");
    let spill = WsFrameSpill::new(&wal_dir).expect("spill new");

    compose("pause").expect("docker pause");
    let down = !questdb_answers(&addr);

    // Capture frames into the WAL while QuestDB is down.
    let live_n = 2_000u32;
    let mut outcomes = Vec::with_capacity(live_n as usize);
    for i in 0..live_n {
        let mut frame = vec![0u8; 16];
        frame[0..4].copy_from_slice(&i.to_le_bytes());
        outcomes.push(spill.append(WsType::LiveFeed, frame));
    }
    let persisted = wait_until_wal_persisted(&spill, u64::from(live_n), Duration::from_secs(10));

    compose("unpause").expect("docker unpause");
    let deadline = Instant::now() + RECOVERY_BUDGET;
    let mut back = questdb_answers(&addr);
    while !back && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        back = questdb_answers(&addr);
    }

    assert!(down, "the pause must take QuestDB at {addr} down");
    assert!(
        outcomes.iter().all(|o| *o == AppendOutcome::Spilled),
        "WAL must capture every frame during the real QuestDB outage"
    );
    assert_eq!(
        persisted,
        u64::from(live_n),
        "WAL must persist every frame during the outage"
    );
    assert_eq!(
        spill.drop_critical_count(),
        0,
        "zero WS-frame loss across a real QuestDB outage"
    );
    assert!(
        back,
        "QuestDB at {addr} must answer again within {RECOVERY_BUDGET:?} of the unpause"
    );
    drop(spill);
    std::thread::sleep(Duration::from_millis(50));
    let recovered = replay_all(&wal_dir).expect("replay");
    assert_eq!(
        recovered.len(),
        live_n as usize,
        "WAL replay must recover every frame"
    );

    let _ = std::fs::remove_dir_all(&wal_dir);
}
