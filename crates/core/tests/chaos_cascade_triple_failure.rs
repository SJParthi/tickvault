//! WAL replay with an expired-token predicate and an optional QuestDB pause.
//!
//! The ordinary cases directly append synthetic frames to the real WAL,
//! close the writer in process, and check complete replayed payloads/types/FIFO
//! identities. The token case calls TokenState predicates; it does not renew
//! a token or inject a WebSocket disconnect.
//!
//! The explicitly selected Docker case pauses the required QuestDB service
//! while the independent WAL captures frames. It never queries or persists a
//! row to QuestDB, so it does not prove database recovery or three simultaneous
//! fault recovery. Missing Docker prerequisites fail that selected fixture.
//!
//! No case here sends SIGKILL, simulates power loss or measures resource leaks.

#![cfg(test)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

#[path = "../../storage/tests/support/owned_wal_replay.rs"]
mod owned_wal_replay;
use owned_wal_replay::{assert_complete_replay, claim_wal};

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tickvault_core::auth::types::{DhanAuthResponseData, TokenState};
use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType};

const N: usize = 5_000;

/// Each fixture uses its own temporary directory.
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

/// Bounded counter poll; callers must check that the target was reached.
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

/// Zero validity establishes an expired token for the public predicates.
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

#[test]
fn cascade_01_wal_roundtrip_with_expired_token_preserves_payloads() {
    let wal_dir = chaos_tmp("wal");
    let token = expired_token();
    assert!(!token.is_valid(), "the token fixture must be expired");
    assert!(
        token.needs_refresh(4),
        "an expired token must require refresh"
    );

    let spill = WsFrameSpill::new(&wal_dir).expect("WsFrameSpill::new");
    for i in 0..N {
        let mut frame = vec![0u8; 16];
        frame[0..4].copy_from_slice(&(i as u32).to_le_bytes());
        assert_eq!(
            spill.append(WsType::LiveFeed, frame),
            AppendOutcome::Spilled,
            "admission of frame {i}"
        );
    }
    assert_eq!(spill.drop_critical_count(), 0);
    assert_eq!(
        wait_until_wal_persisted(&spill, N as u64, Duration::from_secs(10)),
        N as u64,
        "the writer must flush every admitted frame"
    );
    drop(spill);

    let owner = claim_wal(&wal_dir);
    let batch = assert_complete_replay(&owner);
    assert_eq!(batch.frames.len(), N);
    for (index, record) in batch.frames.iter().enumerate() {
        let mut expected = vec![0u8; 16];
        expected[0..4].copy_from_slice(&(index as u32).to_le_bytes());
        assert_eq!(
            record.ws_type,
            WsType::LiveFeed,
            "type at FIFO index {index}"
        );
        assert_eq!(
            record.frame, expected,
            "complete payload at FIFO index {index}"
        );
    }
    assert!(
        batch
            .frames
            .windows(2)
            .all(|pair| pair[0].frame_seq < pair[1].frame_seq),
        "frame identities must increase in captured FIFO order"
    );
    assert!(!token.is_valid() && token.needs_refresh(4));

    let _ = std::fs::remove_dir_all(&wal_dir);
}

#[test]
fn cascade_01_independent_wal_roundtrip_preserves_payloads() {
    let wal_dir = chaos_tmp("indep-wal");
    let spill = WsFrameSpill::new(&wal_dir).expect("spill new");
    for index in 0..1_000u32 {
        assert_eq!(
            spill.append(WsType::OrderUpdate, index.to_le_bytes().to_vec()),
            AppendOutcome::Spilled
        );
    }
    assert_eq!(
        wait_until_wal_persisted(&spill, 1_000, Duration::from_secs(5)),
        1_000,
        "every independently captured record must flush"
    );
    assert_eq!(spill.drop_critical_count(), 0);
    drop(spill);

    let owner = claim_wal(&wal_dir);
    let batch = assert_complete_replay(&owner);
    assert_eq!(batch.frames.len(), 1_000);
    for (index, record) in batch.frames.iter().enumerate() {
        assert_eq!(
            record.ws_type,
            WsType::OrderUpdate,
            "type at FIFO index {index}"
        );
        assert_eq!(
            record.frame,
            (index as u32).to_le_bytes(),
            "complete payload at FIFO index {index}"
        );
    }
    assert!(
        batch
            .frames
            .windows(2)
            .all(|pair| pair[0].frame_seq < pair[1].frame_seq),
        "independent replay must preserve increasing frame identities"
    );
    let _ = std::fs::remove_dir_all(&wal_dir);
}

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
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            stdout.lines().any(|l| l.trim() == QUESTDB_SERVICE)
        }
        _ => false,
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

/// Independent WAL capture while a real, explicitly required QuestDB service
/// is paused. This does not write to QuestDB or test its recovery path.
#[test]
#[ignore = "requires an explicitly configured Docker compose stack"]
fn cascade_01_wal_roundtrip_while_questdb_paused_preserves_payloads() {
    assert!(
        docker_stack_available(),
        "required Docker compose QuestDB service is absent or unavailable; \
         no outage fixture was executed"
    );

    let wal_dir = chaos_tmp("live-wal");
    let spill = WsFrameSpill::new(&wal_dir).expect("spill new");
    let token = expired_token();
    assert!(!token.is_valid() && token.needs_refresh(4));

    // An assertion during capture must still attempt to resume the service.
    // The explicit successful unpause below remains required for a pass.
    struct ResumeOnDrop(bool);
    impl Drop for ResumeOnDrop {
        fn drop(&mut self) {
            if self.0
                && let Err(error) = unpause_questdb()
            {
                eprintln!("fixture cleanup could not unpause QuestDB: {error}");
            }
        }
    }
    pause_questdb().expect("docker pause");
    let mut resume = ResumeOnDrop(true);
    let live_n = 2_000u32;
    for index in 0..live_n {
        let mut frame = vec![0u8; 16];
        frame[0..4].copy_from_slice(&index.to_le_bytes());
        assert_eq!(
            spill.append(WsType::LiveFeed, frame),
            AppendOutcome::Spilled,
            "admission while QuestDB is paused: {index}"
        );
    }
    assert_eq!(
        wait_until_wal_persisted(&spill, u64::from(live_n), Duration::from_secs(10)),
        u64::from(live_n),
        "the WAL must flush every admitted frame before QuestDB resumes"
    );
    unpause_questdb().expect("docker unpause");
    resume.0 = false;
    assert_eq!(spill.drop_critical_count(), 0);
    drop(spill);

    let owner = claim_wal(&wal_dir);
    let batch = assert_complete_replay(&owner);
    assert_eq!(batch.frames.len(), live_n as usize);
    for (index, record) in batch.frames.iter().enumerate() {
        let mut expected = vec![0u8; 16];
        expected[0..4].copy_from_slice(&(index as u32).to_le_bytes());
        assert_eq!(
            record.ws_type,
            WsType::LiveFeed,
            "type at FIFO index {index}"
        );
        assert_eq!(
            record.frame, expected,
            "complete payload at FIFO index {index}"
        );
    }
    assert!(
        batch
            .frames
            .windows(2)
            .all(|pair| pair[0].frame_seq < pair[1].frame_seq),
        "paused-service fixture must preserve increasing frame identities"
    );
    assert!(!token.is_valid() && token.needs_refresh(4));
    let _ = std::fs::remove_dir_all(&wal_dir);
}
