//! The `ensure_*` DDL helpers must DEGRADE, never panic and never hang, when
//! QuestDB is unreachable.
//!
//! # Why this exists
//!
//! Each of these functions runs at boot, returns `()`, and reports failure
//! only through a coded `error!`. That shape has two ways to be wrong and
//! neither is visible from the call site: it can panic (taking the boot down
//! for a table that is best-effort) or it can hang past its own timeout
//! (stalling boot behind a dead database).
//!
//! Nothing exercised those paths. The functions were reachable only against a
//! live QuestDB, so the `Err(err) =>` request-failed arms — the arms that
//! carry the coded errors an operator triages on — had never executed once,
//! in a test or anywhere else.
//!
//! # Why port 1
//!
//! The `boot_probe.rs` precedent: port 1 is unprivileged and always refused,
//! so the connection fails immediately and locally. No network, no fixture, no
//! sleep — the refusal is the test. Every one of these calls must therefore
//! return promptly and quietly.
//!
//! This does NOT assert the log line's content; capturing tracing output would
//! test the subscriber rather than the function. What it pins is the contract
//! the call sites actually depend on: unreachable QuestDB degrades this boot
//! step instead of ending the process.

use std::time::{Duration, Instant};

use tickvault_common::config::QuestDbConfig;

/// A QuestDB that is guaranteed to refuse, immediately and locally.
fn unreachable_questdb() -> QuestDbConfig {
    QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: 1,
        pg_port: 1,
        ilp_port: 1,
    }
}

/// Generous enough that a slow CI runner never flakes, tight enough that a
/// genuinely hanging call (one that waited out its own DDL timeout, or had
/// none) still fails this test.
const MUST_RETURN_WITHIN: Duration = Duration::from_secs(20);

#[tokio::test]
async fn ensure_ws_event_audit_table_degrades_on_unreachable_questdb() {
    let started = Instant::now();
    tickvault_storage::ws_event_audit_persistence::ensure_ws_event_audit_table(
        &unreachable_questdb(),
    )
    .await;
    assert!(
        started.elapsed() < MUST_RETURN_WITHIN,
        "ensure_ws_event_audit_table took {:?} against a refused port — a boot \
         step that waits on a dead database stalls the whole boot behind it",
        started.elapsed()
    );
}

#[tokio::test]
async fn ensure_index_constituency_table_degrades_on_unreachable_questdb() {
    let started = Instant::now();
    tickvault_storage::index_constituency_persistence::ensure_index_constituency_table(
        &unreachable_questdb(),
    )
    .await;
    assert!(
        started.elapsed() < MUST_RETURN_WITHIN,
        "ensure_index_constituency_table took {:?} against a refused port",
        started.elapsed()
    );
}

#[tokio::test]
async fn ensure_instrument_lifecycle_tables_degrade_on_unreachable_questdb() {
    let cfg = unreachable_questdb();
    let started = Instant::now();
    tickvault_storage::instrument_lifecycle_persistence::ensure_instrument_lifecycle_table(&cfg)
        .await;
    tickvault_storage::instrument_lifecycle_persistence::ensure_instrument_lifecycle_audit_table(
        &cfg,
    )
    .await;
    assert!(
        started.elapsed() < MUST_RETURN_WITHIN,
        "the instrument-lifecycle DDL pair took {:?} against a refused port — \
         these are SEBI never-delete tables and their ensure runs every boot",
        started.elapsed()
    );
}
