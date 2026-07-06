//! Source-scan ratchet — WS-GAP-08 persisted 429 cooldown wired into BOTH
//! boot paths (2026-07-06 audit fix).
//!
//! Audit-confirmed HIGH gap: `wait_out_persisted_ws_rate_limit_cooldown`
//! (the WS-GAP-08 boot-time wait that breaks the instant-429 restart loop)
//! had exactly ONE call site — the slow lane (`start_dhan_lane`). The FAST
//! crash-recovery boot arm built + spawned its WS pool with NO cooldown
//! read, so a mid-market `process::exit(2)` with a valid cached token
//! (a 429 does not invalidate the JWT; the WS-GAP-09 ceiling_exceeded
//! fallback still reaches exit(2)) routed through FAST BOOT, wiped the
//! in-memory rate-limit streak, and reconnected at 0ms into a still-active
//! Dhan 429 window — the exact loop WS-GAP-08 exists to break.
//!
//! This guard pins, in main.rs source order per boot path, that EVERY
//! `create_websocket_pool(` call site is preceded by a
//! `wait_out_persisted_ws_rate_limit_cooldown().await` call (mirror of the
//! `wal_reinject::tests::ratchet_tick_processor_spawns_before_reinject_await`
//! house pattern). Runbook: `.claude/rules/project/wave-2-error-codes.md`
//! (WS-GAP-08, 2026-07-06 update).

use std::fs;
use std::path::PathBuf;

fn read_main_rs() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The `().await` suffix distinguishes CALL sites from the fn definition
/// (`async fn wait_out_persisted_ws_rate_limit_cooldown() {`).
const COOLDOWN_CALL: &str = "wait_out_persisted_ws_rate_limit_cooldown().await";
/// Both pool CALL sites are `match create_websocket_pool(`; the fn
/// definition is `fn create_websocket_pool(` and never matches this needle.
const POOL_CALL: &str = "match create_websocket_pool(";

#[test]
fn ratchet_cooldown_wait_precedes_both_pool_spawn_sites() {
    let main_src = read_main_rs();

    let cooldown_positions: Vec<usize> = main_src
        .match_indices(COOLDOWN_CALL)
        .map(|(pos, _)| pos)
        .collect();
    let pool_positions: Vec<usize> = main_src
        .match_indices(POOL_CALL)
        .map(|(pos, _)| pos)
        .collect();

    assert_eq!(
        pool_positions.len(),
        2,
        "expected exactly 2 `{POOL_CALL}` call sites in main.rs (fast \
         crash-recovery boot + slow lane); found {} — update this ratchet's \
         pairing logic if a boot path was added/removed",
        pool_positions.len()
    );
    assert_eq!(
        cooldown_positions.len(),
        2,
        "expected exactly 2 `{COOLDOWN_CALL}` call sites in main.rs (one per \
         boot path); found {} — the WS-GAP-08 persisted 429 cooldown must be \
         honoured before EVERY Dhan main-feed pool creation (2026-07-06 \
         audit fix: the fast crash-recovery arm previously skipped it and \
         reconnected at 0ms into a still-active 429 window)",
        cooldown_positions.len()
    );

    for (i, (cooldown, pool)) in cooldown_positions
        .iter()
        .zip(pool_positions.iter())
        .enumerate()
    {
        assert!(
            cooldown < pool,
            "boot path #{i}: {COOLDOWN_CALL} at byte {cooldown} must precede \
             `{POOL_CALL}` at byte {pool} — the persisted Dhan 429 cooldown \
             must be waited out BEFORE the pool is created/spawned, or a \
             process::exit(2) restart reconnects at 0ms into the still-active \
             429 window (the WS-GAP-08 instant-429 restart loop)"
        );
        // Per-path pairing sanity: the i-th cooldown call must belong to the
        // i-th boot path — it must come AFTER the previous path's pool spawn.
        if i > 0 {
            assert!(
                cooldown > &pool_positions[i - 1],
                "boot path #{i}: {COOLDOWN_CALL} at byte {cooldown} sits before \
                 the previous path's pool spawn at byte {} — the pairing is \
                 broken; each boot path needs its OWN cooldown wait",
                pool_positions[i - 1]
            );
        }
    }
}

/// The wait itself must stay wired to the persisted cooldown reader (the
/// fail-open file read + bounded sleep), not degrade to a stub.
#[test]
fn ratchet_cooldown_fn_reads_persisted_file_and_is_bounded() {
    let main_src = read_main_rs();
    assert!(
        main_src.contains("rate_limit_cooldown::read_cooldown()"),
        "wait_out_persisted_ws_rate_limit_cooldown must read the persisted \
         cooldown file via rate_limit_cooldown::read_cooldown() (fail-open)"
    );
    assert!(
        main_src.contains("WS_RATE_LIMIT_BACKOFF_CAP_MS"),
        "wait_out_persisted_ws_rate_limit_cooldown must clamp the wait to \
         WS_RATE_LIMIT_BACKOFF_CAP_MS so a bad file can never hang boot \
         beyond the 5-minute cap"
    );
}
