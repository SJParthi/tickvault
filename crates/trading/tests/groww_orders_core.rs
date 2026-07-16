//! ORD-PR-2 cross-module integration: the Groww orders PURE CORE exercised
//! through the public `oms::groww` surface — reference-id generation → the
//! fsynced intent ledger → a simulated kill-9 → boot replay classification →
//! reconcile against a broker snapshot. Compiles ONLY under the
//! `groww_orders` feature (Gate 2 — the default build contains none of this).
#![cfg(feature = "groww_orders")]

use std::io::Write as _;
use std::path::{Path, PathBuf};

use tickvault_trading::oms::groww::intent_ledger::{
    BootIntentClass, IntentKind, IntentLedger, IntentPhase, NewIntent,
};
use tickvault_trading::oms::groww::reconcile::{
    CrossCloseAction, GhostBrokerAction, LocalOrderView, MismatchKind, ReconcileParams,
    SnapshotRow, classify_cross_close_intent, classify_cross_close_open_intents,
    reconcile_groww_orders,
};
use tickvault_trading::oms::groww::reference_id::{IstDate, generate_reference_id, is_ours};
use tickvault_trading::oms::groww::state::TrackedOrderState;
use tickvault_trading::oms::groww::types::{
    GrowwCreateOrderReq, GrowwExchange, GrowwOrderStatus, GrowwOrderType, GrowwProduct,
    GrowwSegment, GrowwTransactionType, GrowwValidity, validate_create_order,
};

const DATE: IstDate = IstDate {
    year: 2026,
    month: 7,
    day: 15,
};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tv-groww-ord-pr2-it-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// The full happy-path chain: validate → journal → send phases → settle,
/// then reconcile the tracked order against a broker snapshot.
#[test]
fn test_place_chain_validate_journal_track_reconcile() {
    let dir = temp_dir("chain");
    let reference = generate_reference_id(DATE, 1, 42);
    assert!(is_ours(&reference));

    // 1. Pure financial validation.
    let req = GrowwCreateOrderReq {
        trading_symbol: "NIFTY".to_owned(),
        quantity: 1,
        price: Some(24_550_00), // ₹24,550.00 in paise
        trigger_price: None,
        validity: GrowwValidity::Day,
        exchange: GrowwExchange::Nse,
        segment: GrowwSegment::Fno,
        product: GrowwProduct::Nrml,
        order_type: GrowwOrderType::Limit,
        transaction_type: GrowwTransactionType::Buy,
        order_reference_id: reference.clone(),
    };
    validate_create_order(&req, 1).unwrap();

    // 2. Write-ahead journal — receipt proves the fsynced `recorded` line.
    let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
    let receipt = ledger
        .record_intent(
            NewIntent {
                intent_id: reference.clone(),
                reference_id: reference.clone(),
                kind: IntentKind::Place,
                groww_order_id: None,
                mode: "paper".to_owned(),
                ts_ms: 1,
                linked_intent_id: None,
            },
            DATE,
        )
        .unwrap();
    ledger
        .append_phase_for_receipt(&receipt, IntentPhase::Sent, 2, DATE, None, None)
        .unwrap();
    ledger
        .append_phase_for_receipt(
            &receipt,
            IntentPhase::Acked,
            3,
            DATE,
            Some("GRW1".to_owned()),
            None,
        )
        .unwrap();
    assert_eq!(ledger.open_intent_count(), 0);

    // 3. Reconcile the now-tracked order against a snapshot that moved it
    //    forward (poll-skip: OPEN → EXECUTED in one observed edge).
    let locals = [LocalOrderView {
        groww_order_id: "GRW1".to_owned(),
        reference_id: reference.clone(),
        state: TrackedOrderState::new(GrowwOrderStatus::Open, 0),
        trading_date_yymmdd: DATE.yymmdd_num(),
        created_ts_ms: 1,
        missing_sweeps: 0,
    }];
    let snapshot = [
        SnapshotRow {
            groww_order_id: Some("GRW1".to_owned()),
            order_status: Some("EXECUTED".to_owned()),
            filled_quantity: Some(1),
            order_reference_id: Some(reference),
        },
        // A BruteX co-tenant row on the shared account: counted + ignored.
        SnapshotRow {
            groww_order_id: Some("GRWFOREIGN".to_owned()),
            order_status: Some("OPEN".to_owned()),
            filled_quantity: Some(0),
            order_reference_id: Some("BRUTEX-01-XY".to_owned()),
        },
    ];
    let report = reconcile_groww_orders(
        &locals,
        &snapshot,
        &[],
        ReconcileParams {
            now_ms: 100_000,
            sweep_interval_ms: 60_000,
            today_yymmdd: DATE.yymmdd_num(),
        },
    );
    assert_eq!(report.orders["GRW1"].finding, None);
    assert_eq!(report.foreign_ignored, 1);
    assert_eq!(report.ghost_broker.len(), 1);
    assert_eq!(
        report.ghost_broker[0].action,
        GhostBrokerAction::ForeignIgnored
    );
    cleanup(&dir);
}

/// Kill-9 mid-append + restart: the torn tail is tolerated, the in-flight
/// intent classifies for the boot ladder, and a NEW place on the same
/// reference stays refused until the ladder settles it.
#[test]
fn test_kill9_replay_blocks_new_mutations_until_resolution() {
    let dir = temp_dir("kill9");
    let reference = generate_reference_id(DATE, 5, 9);
    let path;
    {
        let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
        let receipt = ledger
            .record_intent(
                NewIntent {
                    intent_id: reference.clone(),
                    reference_id: reference.clone(),
                    kind: IntentKind::Place,
                    groww_order_id: None,
                    mode: "paper".to_owned(),
                    ts_ms: 1,
                    linked_intent_id: None,
                },
                DATE,
            )
            .unwrap();
        ledger
            .append_phase_for_receipt(&receipt, IntentPhase::Sent, 2, DATE, None, None)
            .unwrap();
        path = ledger.path().to_path_buf();
        // kill -9 here (no clean shutdown; the next append never happened).
    }
    // A torn fragment from the killed process.
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(br#"{"intent_id":"TV2607"#).unwrap();
    }
    // Boot replay.
    let mut ledger = IntentLedger::open(&dir, DATE).unwrap();
    assert!(ledger.torn_tail_tolerated());
    let open = ledger.classify_open_intents();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].1, BootIntentClass::NeedsResolution);
    // Sequence resumes ABOVE the replayed one (crash-safe monotone).
    assert_eq!(ledger.max_sequence(), Some(5));
    // The same logical order is refused while the intent is unresolved
    // (the F-1 pin: refusal rides the OPEN intent).
    let dup = ledger.record_intent(
        NewIntent {
            intent_id: generate_reference_id(DATE, 6, 1),
            reference_id: reference.clone(),
            kind: IntentKind::Place,
            groww_order_id: None,
            mode: "paper".to_owned(),
            ts_ms: 3,
            linked_intent_id: None,
        },
        DATE,
    );
    assert!(
        dup.is_err(),
        "open intent must block a same-reference place"
    );
    // The ladder settles it as landed → the block lifts for a NEW order id.
    ledger
        .append_phase(
            &reference,
            IntentPhase::ResolvedLanded,
            4,
            DATE,
            Some("GRW7".to_owned()),
            None,
        )
        .unwrap();
    assert_eq!(ledger.open_intent_count(), 0);
    cleanup(&dir);
}

/// Cross-close: a prior-day open intent with UNPROVEN prior-day readability
/// routes to Critical — never a silent Expired stamp (doc-F1).
#[test]
fn test_cross_close_prior_day_unreadable_routes_critical() {
    assert_eq!(
        classify_cross_close_intent(260_714, 260_715, BootIntentClass::NeedsResolution, false),
        CrossCloseAction::MarkUnresolvedCritical
    );
    assert_eq!(
        classify_cross_close_intent(260_715, 260_715, BootIntentClass::NeedsResolution, false),
        CrossCloseAction::SameDayBootProtocol
    );
}

/// Ghost-local across two sweeps through the PUBLIC reconcile surface.
#[test]
fn test_ghost_local_two_sweep_grace_via_public_surface() {
    let params = ReconcileParams {
        now_ms: 1_000_000,
        sweep_interval_ms: 60_000,
        today_yymmdd: DATE.yymmdd_num(),
    };
    let mut lo = LocalOrderView {
        groww_order_id: "G1".to_owned(),
        reference_id: generate_reference_id(DATE, 2, 2),
        state: TrackedOrderState::new(GrowwOrderStatus::Open, 0),
        trading_date_yymmdd: DATE.yymmdd_num(),
        created_ts_ms: 100_000, // well older than one sweep interval
        missing_sweeps: 0,
    };
    let r1 = reconcile_groww_orders(std::slice::from_ref(&lo), &[], &[], params);
    assert_eq!(r1.orders["G1"].finding, None, "sweep 1 = grace");
    lo.missing_sweeps = r1.orders["G1"].next_missing_sweeps;
    let r2 = reconcile_groww_orders(&[lo], &[], &[], params);
    assert_eq!(
        r2.orders["G1"].finding,
        Some(MismatchKind::GhostLocal),
        "sweep 2 confirms"
    );
}

/// HIGH-4 end-to-end: a prior-day session's OPEN intents survive the day
/// rollover through the REAL producer chain — multi-file ledger replay →
/// `classify_open_intents` → `classify_cross_close_open_intents` — and
/// route to Critical while prior-day readability is unproven.
#[test]
fn test_day_rollover_ledger_feeds_cross_close_producer() {
    let dir = temp_dir("rollover-chain");
    let day1 = IstDate {
        year: 2026,
        month: 7,
        day: 14,
    };
    let day1_rid = generate_reference_id(day1, 3, 3);
    {
        let mut l1 = IntentLedger::open(&dir, day1).unwrap();
        let receipt = l1
            .record_intent(
                NewIntent {
                    intent_id: day1_rid.clone(),
                    reference_id: day1_rid.clone(),
                    kind: IntentKind::Place,
                    groww_order_id: None,
                    mode: "paper".to_owned(),
                    ts_ms: 1,
                    linked_intent_id: None,
                },
                day1,
            )
            .unwrap();
        l1.append_phase_for_receipt(&receipt, IntentPhase::Sent, 2, day1, None, None)
            .unwrap();
        // kill -9 overnight.
    }
    // Next-morning boot on DAY 2: the directory scan replays day 1.
    let mut l2 = IntentLedger::open(&dir, DATE).unwrap();
    assert_eq!(l2.max_sequence(), Some(3), "sequence spans the rollover");
    let open = l2.classify_open_intents();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].1, BootIntentClass::NeedsResolution);
    // The REAL producer routes the prior-day open intent fail-closed.
    let actions = classify_cross_close_open_intents(&open, DATE, false);
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].1, CrossCloseAction::MarkUnresolvedCritical);
    // Once day-0 probe D-10 proves prior-day reads, it routes per-order.
    let actions = classify_cross_close_open_intents(&open, DATE, true);
    assert_eq!(actions[0].1, CrossCloseAction::ResolvePerOrderReads);
    // The prior-day open place still blocks its reference id TODAY.
    assert!(
        l2.record_intent(
            NewIntent {
                intent_id: generate_reference_id(DATE, 4, 4),
                reference_id: day1_rid,
                kind: IntentKind::Place,
                groww_order_id: None,
                mode: "paper".to_owned(),
                ts_ms: 9,
                linked_intent_id: None,
            },
            DATE,
        )
        .is_err(),
        "yesterday's open intent must block a same-reference place today"
    );
    cleanup(&dir);
}
