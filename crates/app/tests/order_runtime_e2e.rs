//! Order-runtime dry-run PR (2026-07-14) — black-box integration test.
//!
//! Exercises the SPAWNED runtime end-to-end through its public seams (the
//! order-update broadcast + the mark mpsc + the shared `marks_wanted` flag)
//! with NO live QuestDB / Dhan / Telegram (NotificationService::disabled()):
//!
//! 1. the supervisor + inner task come up and stay alive;
//! 2. an ORPHAN fill-carrying update (the WAL-replay-across-restart shape)
//!    is tolerated loudly — no panic, no position, `marks_wanted` stays
//!    disarmed (an empty paper book must not tax the Groww per-tick path);
//! 3. the mark arm DRAINS: far more marks than the channel capacity are
//!    accepted over time (a dead mark arm would wedge the channel FULL);
//! 4. foreign-source (Source=N — the operator's manual Dhan-app orders)
//!    events never arm the book either.
//!
//! The white-box state machine (fills, paper filler, tripwire, reconcile,
//! self-test, daily reset) is covered by the unit suite in
//! `crates/app/src/order_runtime.rs`; the wiring ordering is pinned by
//! `test_rest_stack_wires_order_runtime` + `order_runtime_spawn_site_guard`.
//!
//! H3 (fix-round 2026-07-14): the FULL promised chain — place → paper fill
//! via a mark → net_lots ≠ 0 → adverse mark → daily-loss halt → RiskHalt
//! reaches a test alert sink → check_order rejects — is driven through the
//! ACTUAL production arm bodies (`process_mark` etc.) by
//! `order_runtime::tests::test_e2e_place_fill_mark_halt_fires_risk_halt_sink`
//! (an in-module test: the arm bodies are private, and the SPAWNED runtime
//! only places orders via the time-gated self-test, which no test can drive
//! deterministically). THIS file keeps the spawned-task black-box coverage
//! (liveness, orphan tolerance, drain, disarmed gate).

#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tickvault_app::order_runtime::{MarkUpdate, OrderRuntimeParams, spawn_order_runtime};
use tickvault_common::config::ApplicationConfig;
use tickvault_common::order_types::OrderUpdate;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::notification::NotificationService;

fn load_base_config() -> ApplicationConfig {
    use figment::Figment;
    use figment::providers::{Format, Toml};
    // Integration tests run with cwd = workspace root (Cargo convention);
    // fall back to the crate-relative path.
    let config_path = if std::path::Path::new("config/base.toml").exists() {
        "config/base.toml"
    } else if std::path::Path::new("../../config/base.toml").exists() {
        "../../config/base.toml"
    } else {
        panic!("config/base.toml not found from workspace root or crate directory")
    };
    let mut config: ApplicationConfig = Figment::new()
        .merge(Toml::file(config_path))
        .extract()
        .expect("config/base.toml must parse");
    // Deterministic test posture (the runtime under test is time-gated for
    // reconcile/self-test — this e2e exercises the event arms only).
    config.order_runtime.enabled = true;
    config.order_runtime.self_test = false;
    config
}

/// A fill-carrying TRADED update for an order id the fresh paper book does
/// not track (the orphan / WAL-replay shape). Built via serde defaults so
/// the 40-field struct stays maintainable here.
fn orphan_traded_update(source: &str) -> OrderUpdate {
    let mut update: OrderUpdate =
        serde_json::from_str("{}").expect("all OrderUpdate fields are serde-default");
    update.exchange = "NSE".to_string();
    update.segment = "D".to_string();
    update.security_id = "49081".to_string();
    update.order_no = "GHOST-1".to_string();
    update.txn_type = "B".to_string();
    update.status = "TRADED".to_string();
    update.quantity = 75;
    update.traded_qty = 75;
    update.avg_traded_price = 123.45;
    update.lot_size = 75;
    update.source = source.to_string();
    update
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn order_runtime_e2e_orphan_updates_and_mark_drain() {
    let config = Arc::new(load_base_config());
    let calendar = Arc::new(
        TradingCalendar::from_config(&config.trading)
            .expect("base.toml trading calendar must build"),
    );
    let notifier = NotificationService::disabled();

    let (order_update_sender, first_rx) = tokio::sync::broadcast::channel::<OrderUpdate>(256);
    const MARK_CAPACITY: usize = 64;
    let (mark_tx, mark_rx) = tokio::sync::mpsc::channel::<MarkUpdate>(MARK_CAPACITY);
    let marks_wanted = Arc::new(AtomicBool::new(false));
    let auth_notify = Arc::new(tokio::sync::Notify::new());
    let token_handle: tickvault_core::auth::token_manager::TokenHandle =
        Arc::new(arc_swap::ArcSwap::from_pointee(None));

    let supervisor = spawn_order_runtime(OrderRuntimeParams {
        config,
        notifier,
        calendar,
        order_update_sender: order_update_sender.clone(),
        first_order_update_rx: first_rx,
        mark_rx,
        marks_wanted: Arc::clone(&marks_wanted),
        token_handle,
        client_id: "1106656882".to_string(),
        auth_notify,
    });

    // (2) Orphan fill-carrying update: tolerated loudly, never a position.
    order_update_sender
        .send(orphan_traded_update("P"))
        .expect("runtime receiver must be alive");
    // (4) Foreign-source event: filtered at the boundary.
    order_update_sender
        .send(orphan_traded_update("N"))
        .expect("runtime receiver must be alive");

    // (3) Mark arm drains: push 4x the channel capacity. A dead/wedged mark
    // arm would leave the channel FULL and time this out; a draining arm
    // accepts all of them (marks for an empty book are consumed + skipped).
    let mut sent = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while sent < MARK_CAPACITY * 4 {
        match mark_tx.try_send(MarkUpdate {
            security_id: 49_081,
            segment_code: 2,
            price: 123.5,
        }) {
            Ok(()) => sent += 1,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "mark channel stayed FULL — the runtime's mark arm is not draining"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                panic!("mark channel closed — the runtime died mid-test");
            }
        }
    }

    // Give the runtime a beat to fold the tail, then assert the observable
    // contract: empty book => the Groww per-tick gate stays DISARMED, and
    // the runtime is still alive (supervisor never resolved).
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !marks_wanted.load(Ordering::Relaxed),
        "orphan/foreign updates must never create a position — an empty paper \
         book must leave the Groww per-tick mark gate DISARMED"
    );
    assert!(
        !supervisor.is_finished(),
        "the order-runtime supervisor must stay alive (it only exits on \
         runtime-shutdown cancellation)"
    );

    supervisor.abort();
}

/// Fix F (2026-07-17 respawn flap): the mark producers are DAY-SCOPED — the
/// Groww per-minute REST legs' supervisors exit at day completion (~15:31
/// IST, after the post-session sweep) and drop the last `MarkForwarder`
/// clones, closing the mark mpsc for the rest of the process lifetime. The
/// runtime must treat that close as a steady state: disarm the mark arm and
/// KEEP LOOPING. Pre-fix, Arm 2 `return`ed on the closed channel; the
/// supervisor classified the exit `clean_exit` and respawned an inner that
/// re-read the SAME closed channel and exited again within milliseconds — a
/// permanent respawn flap (escalating to the 300s backoff cap) that reset
/// the paper book on every step, observed live from 15:31 IST 2026-07-17
/// through post-close and again from the fresh 16:08 deploy.
///
/// The observable contract: after the mark sender drops, the inner task must
/// stay alive — the order-update broadcast keeps its ONE receiver, so
/// `send()` keeps succeeding (with the bug, the dead inner drops its
/// receiver and `send()` errors during the respawn backoff window).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn order_runtime_e2e_mark_channel_close_does_not_exit_the_loop() {
    let config = Arc::new(load_base_config());
    let calendar = Arc::new(
        TradingCalendar::from_config(&config.trading)
            .expect("base.toml trading calendar must build"),
    );
    let notifier = NotificationService::disabled();

    let (order_update_sender, first_rx) = tokio::sync::broadcast::channel::<OrderUpdate>(256);
    let (mark_tx, mark_rx) = tokio::sync::mpsc::channel::<MarkUpdate>(64);
    let marks_wanted = Arc::new(AtomicBool::new(false));
    let auth_notify = Arc::new(tokio::sync::Notify::new());
    let token_handle: tickvault_core::auth::token_manager::TokenHandle =
        Arc::new(arc_swap::ArcSwap::from_pointee(None));

    let supervisor = spawn_order_runtime(OrderRuntimeParams {
        config,
        notifier,
        calendar,
        order_update_sender: order_update_sender.clone(),
        first_order_update_rx: first_rx,
        mark_rx,
        marks_wanted: Arc::clone(&marks_wanted),
        token_handle,
        client_id: "1106656882".to_string(),
        auth_notify,
    });

    // Prove the runtime is up first (the receiver is alive).
    order_update_sender
        .send(orphan_traded_update("P"))
        .expect("runtime receiver must be alive before the mark-channel close");

    // Day end: the last day-scoped mark producer drops its sender.
    drop(mark_tx);

    // Pre-fix the inner task observed the close and returned within
    // milliseconds, dropping its broadcast receiver (the supervisor then sat
    // in its 5s respawn backoff with ZERO receivers). Post-fix the inner
    // disarms the mark arm and keeps looping — sends keep succeeding.
    tokio::time::sleep(Duration::from_millis(500)).await;
    for _ in 0..3 {
        assert!(
            order_update_sender.send(orphan_traded_update("P")).is_ok(),
            "order-update send failed after the mark channel closed — the \
             runtime inner task exited (the clean_exit respawn flap)"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !supervisor.is_finished(),
        "the order-runtime supervisor must stay alive after the mark \
         channel closes"
    );

    supervisor.abort();
}
