// Included into `executor.rs`'s `#[cfg(test)] mod tests` via `include!`.
// Mock-transport integration for the permutation-table transport rows + the
// ORD-PR-3 carry-note ratchets (design §5).

use super::*;
use crate::oms::groww::api_client::{AmbiguityReason, OrderTransport, TransportOutcome};
use crate::oms::groww::intent_ledger::{IntentLedger, IntentReceipt};
use crate::oms::groww::reference_id::IstDate;
use crate::oms::groww::types::{
    GrowwCancelOrderReq, GrowwCreateOrderReq, GrowwExchange, GrowwModifyOrderReq,
    GrowwMutationRespPayload, GrowwOrderDetailPayload, GrowwOrderStatusPayload, GrowwOrderType,
    GrowwProduct, GrowwSegment, GrowwTradeRow, GrowwTransactionType, GrowwValidity,
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;

const DATE: IstDate = IstDate {
    year: 2026,
    month: 7,
    day: 15,
};
const NOW_MS: i64 = 1_752_000_000_000;

fn temp_ledger_dir(tag: &str) -> PathBuf {
    // Unique per call — the parallel test runner must never share a ledger dir
    // (a concurrent open/remove race across same-tag tests).
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("tv-groww-ord-pr3-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn open_ledger(tag: &str) -> IntentLedger {
    IntentLedger::open(&temp_ledger_dir(tag), DATE).expect("ledger open")
}

fn cfg() -> ExecutorConfig {
    ExecutorConfig {
        max_order_quantity: 100,
        ambiguity_ladder_max_secs: AMBIGUITY_LADDER_MAX_SECS,
        replay_policy_auto: true,
        live_fire_requested: true, // inert without GROWW_ORDER_LIVE_FIRE
    }
}

fn base_req() -> GrowwCreateOrderReq {
    GrowwCreateOrderReq {
        trading_symbol: "NIFTY-FUT".to_owned(),
        quantity: 50,
        price: Some(2_500_00),
        trigger_price: None,
        validity: GrowwValidity::Day,
        exchange: GrowwExchange::Nse,
        segment: GrowwSegment::Fno,
        product: GrowwProduct::Nrml,
        order_type: GrowwOrderType::Limit,
        transaction_type: GrowwTransactionType::Buy,
        order_reference_id: "PLACEHOLDER0000".to_owned(),
    }
}

fn mut_ok(id: &str) -> TransportOutcome<GrowwMutationRespPayload> {
    TransportOutcome::Success(GrowwMutationRespPayload {
        groww_order_id: Some(id.to_owned()),
        order_status: Some("OPEN".to_owned()),
        order_reference_id: None,
        remark: None,
    })
}

fn status_ok(id: &str, status: &str, filled: i64) -> TransportOutcome<GrowwOrderStatusPayload> {
    TransportOutcome::Success(GrowwOrderStatusPayload {
        groww_order_id: Some(id.to_owned()),
        order_status: Some(status.to_owned()),
        remark: None,
        filled_quantity: Some(filled),
        order_reference_id: None,
    })
}

fn detail(status: &str, filled: i64) -> TransportOutcome<GrowwOrderDetailPayload> {
    let d = GrowwOrderDetailPayload {
        groww_order_id: Some("GW-1".to_owned()),
        trading_symbol: None,
        order_status: Some(status.to_owned()),
        remark: None,
        quantity: Some(50),
        price: None,
        trigger_price: None,
        filled_quantity: Some(filled),
        remaining_quantity: None,
        average_fill_price: None,
        deliverable_quantity: None,
        amo_status: None,
        validity: None,
        exchange: None,
        order_type: None,
        transaction_type: None,
        segment: None,
        product: None,
        created_at: None,
        exchange_time: None,
        trade_date: None,
        order_reference_id: None,
    };
    TransportOutcome::Success(d)
}

/// A scriptable + counting transport for the ladder integration tests.
#[derive(Default)]
struct ScriptState {
    create: VecDeque<TransportOutcome<GrowwMutationRespPayload>>,
    modify: VecDeque<TransportOutcome<GrowwMutationRespPayload>>,
    cancel: VecDeque<TransportOutcome<GrowwMutationRespPayload>>,
    status_ref: VecDeque<TransportOutcome<GrowwOrderStatusPayload>>,
    detail: VecDeque<TransportOutcome<GrowwOrderDetailPayload>>,
    create_refs: Vec<String>,
    status_ref_refs: Vec<String>,
    detail_ids: Vec<String>,
}

struct ScriptedTransport {
    st: StdMutex<ScriptState>,
}

impl ScriptedTransport {
    fn new(st: ScriptState) -> Self {
        Self {
            st: StdMutex::new(st),
        }
    }
    fn create_refs(&self) -> Vec<String> {
        self.st.lock().unwrap().create_refs.clone()
    }
    fn status_ref_refs(&self) -> Vec<String> {
        self.st.lock().unwrap().status_ref_refs.clone()
    }
}

impl OrderTransport for ScriptedTransport {
    async fn create_order(
        &self,
        req: &GrowwCreateOrderReq,
        _token: &SecretString,
        _receipt: &IntentReceipt,
    ) -> TransportOutcome<GrowwMutationRespPayload> {
        let mut st = self.st.lock().unwrap();
        st.create_refs.push(req.order_reference_id.clone());
        st.create
            .pop_front()
            .unwrap_or(TransportOutcome::Ambiguous(AmbiguityReason::Timeout))
    }
    async fn modify_order(
        &self,
        _req: &GrowwModifyOrderReq,
        _token: &SecretString,
        _receipt: &IntentReceipt,
    ) -> TransportOutcome<GrowwMutationRespPayload> {
        self.st
            .lock()
            .unwrap()
            .modify
            .pop_front()
            .unwrap_or(TransportOutcome::Ambiguous(AmbiguityReason::Timeout))
    }
    async fn cancel_order(
        &self,
        _req: &GrowwCancelOrderReq,
        _token: &SecretString,
        _receipt: &IntentReceipt,
    ) -> TransportOutcome<GrowwMutationRespPayload> {
        self.st
            .lock()
            .unwrap()
            .cancel
            .pop_front()
            .unwrap_or(TransportOutcome::Ambiguous(AmbiguityReason::Timeout))
    }
    async fn get_status_by_id(
        &self,
        _id: &str,
        _segment: GrowwSegment,
        _token: &SecretString,
    ) -> TransportOutcome<GrowwOrderStatusPayload> {
        TransportOutcome::Ambiguous(AmbiguityReason::Timeout)
    }
    async fn get_status_by_reference(
        &self,
        order_reference_id: &str,
        _segment: GrowwSegment,
        _token: &SecretString,
    ) -> TransportOutcome<GrowwOrderStatusPayload> {
        let mut st = self.st.lock().unwrap();
        st.status_ref_refs.push(order_reference_id.to_owned());
        st.status_ref
            .pop_front()
            .unwrap_or(TransportOutcome::Ambiguous(AmbiguityReason::Timeout))
    }
    async fn get_order_detail(
        &self,
        id: &str,
        _segment: GrowwSegment,
        _token: &SecretString,
    ) -> TransportOutcome<GrowwOrderDetailPayload> {
        let mut st = self.st.lock().unwrap();
        st.detail_ids.push(id.to_owned());
        st.detail
            .pop_front()
            .unwrap_or(TransportOutcome::Ambiguous(AmbiguityReason::Timeout))
    }
    async fn list_orders(
        &self,
        _segment: GrowwSegment,
        _page: u32,
        _page_size: u32,
        _token: &SecretString,
    ) -> TransportOutcome<Vec<GrowwOrderDetailPayload>> {
        TransportOutcome::Success(Vec::new())
    }
    async fn get_trades(
        &self,
        _id: &str,
        _segment: GrowwSegment,
        _page: u32,
        _page_size: u32,
        _token: &SecretString,
    ) -> TransportOutcome<Vec<GrowwTradeRow>> {
        TransportOutcome::Success(Vec::new())
    }
}

fn live_exec(st: ScriptState) -> GrowwOrderExecutor<ScriptedTransport> {
    GrowwOrderExecutor::new_live_for_test(
        ScriptedTransport::new(st),
        SecretString::from("test-token"),
        cfg(),
        open_ledger("live"),
    )
}

// ---------------------------------------------------------------------------
// P: place happy paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn place_success_adopts_order_id() {
    let st = ScriptState {
        create: VecDeque::from([mut_ok("GW-1")]),
        ..Default::default()
    };
    let mut exec = live_exec(st);
    let out = exec
        .place_order(base_req(), DATE, NOW_MS, true)
        .await
        .unwrap();
    assert_eq!(
        out,
        PlaceResult::Accepted {
            order_id: "GW-1".to_owned()
        }
    );
    assert!(exec.tracked_order("GW-1").is_some());
}

#[tokio::test]
async fn paper_place_is_zero_http_and_accepts() {
    let mut exec = GrowwOrderExecutor::new_paper(open_ledger("paper"), cfg());
    assert_eq!(exec.mode(), ExecutionMode::Paper);
    let out = exec
        .place_order(base_req(), DATE, NOW_MS, true)
        .await
        .unwrap();
    match out {
        PlaceResult::PaperAccepted { order_id } => assert!(order_id.starts_with("PAPER-")),
        other => panic!("expected PaperAccepted, got {other:?}"),
    }
}

#[tokio::test]
async fn place_well_shaped_reject_is_rejected() {
    let st = ScriptState {
        create: VecDeque::from([TransportOutcome::Rejected {
            http_status: 400,
            ga_code: Some("GA001".to_owned()),
            message: Some("bad".to_owned()),
        }]),
        ..Default::default()
    };
    let mut exec = live_exec(st);
    let out = exec
        .place_order(base_req(), DATE, NOW_MS, true)
        .await
        .unwrap();
    assert_eq!(
        out,
        PlaceResult::Rejected {
            ga_code: Some("GA001".to_owned())
        }
    );
}

// ---------------------------------------------------------------------------
// P17: 429 on place → FULL ladder, SAME reference id, NO re-place
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn place_429_enters_full_ladder_same_ref_no_replace() {
    let st = ScriptState {
        create: VecDeque::from([TransportOutcome::RateLimited {
            http_status: 429,
            retry_after_secs: Some(5),
            body_excerpt: None,
        }]),
        status_ref: VecDeque::from([status_ok("GW-9", "OPEN", 0)]),
        ..Default::default()
    };
    let mut exec = live_exec(st);
    let out = exec
        .place_order(base_req(), DATE, NOW_MS, true)
        .await
        .unwrap();
    assert_eq!(
        out,
        PlaceResult::ResolvedLanded {
            order_id: "GW-9".to_owned()
        }
    );
    // Only ONE create was issued (429 never re-places); the ladder polled the
    // status-by-reference endpoint with the SAME reference id.
    let create_refs = exec.transport.create_refs();
    let ref_refs = exec.transport.status_ref_refs();
    assert_eq!(create_refs.len(), 1, "429 must not re-place");
    assert!(!ref_refs.is_empty());
    assert!(
        ref_refs.iter().all(|r| *r == create_refs[0]),
        "ladder must poll the SAME ref"
    );
}

// ---------------------------------------------------------------------------
// P15: place timeout(ambiguous) → resolve landed
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn place_timeout_ambiguous_resolves_landed() {
    let st = ScriptState {
        create: VecDeque::from([TransportOutcome::Ambiguous(AmbiguityReason::Timeout)]),
        status_ref: VecDeque::from([status_ok("GW-7", "OPEN", 0)]),
        ..Default::default()
    };
    let mut exec = live_exec(st);
    let out = exec
        .place_order(base_req(), DATE, NOW_MS, true)
        .await
        .unwrap();
    assert_eq!(
        out,
        PlaceResult::ResolvedLanded {
            order_id: "GW-7".to_owned()
        }
    );
}

// ---------------------------------------------------------------------------
// P15/P30: place timeout → not-landed → replay (SAME ref) → landed
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn place_not_landed_replays_same_ref_then_lands() {
    let st = ScriptState {
        create: VecDeque::from([
            TransportOutcome::Ambiguous(AmbiguityReason::Timeout), // initial send
            mut_ok("GW-REPLAY"),                                   // the replay send lands
        ]),
        status_ref: VecDeque::from([TransportOutcome::Rejected {
            http_status: 404,
            ga_code: Some("GA004".to_owned()),
            message: None,
        }]),
        ..Default::default()
    };
    let mut exec = live_exec(st);
    let out = exec
        .place_order(base_req(), DATE, NOW_MS, true)
        .await
        .unwrap();
    assert_eq!(
        out,
        PlaceResult::ResolvedLanded {
            order_id: "GW-REPLAY".to_owned()
        }
    );
    let create_refs = exec.transport.create_refs();
    assert_eq!(create_refs.len(), 2, "one initial + one replay send");
    assert_eq!(
        create_refs[0], create_refs[1],
        "replay must reuse the SAME reference id"
    );
}

#[tokio::test(start_paused = true)]
async fn place_not_landed_no_replay_when_policy_off() {
    let mut c = cfg();
    c.replay_policy_auto = false;
    let st = ScriptState {
        create: VecDeque::from([TransportOutcome::Ambiguous(AmbiguityReason::Timeout)]),
        status_ref: VecDeque::from([TransportOutcome::Rejected {
            http_status: 404,
            ga_code: Some("GA004".to_owned()),
            message: None,
        }]),
        ..Default::default()
    };
    let mut exec = GrowwOrderExecutor::new_live_for_test(
        ScriptedTransport::new(st),
        SecretString::from("t"),
        c,
        open_ledger("noreplay"),
    );
    let out = exec
        .place_order(base_req(), DATE, NOW_MS, true)
        .await
        .unwrap();
    assert_eq!(out, PlaceResult::ResolvedNotLanded);
}

// ---------------------------------------------------------------------------
// P25: auth-stale PAUSES the ladder clock (does not exhaust)
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn auth_stale_pauses_clock_and_still_lands() {
    let st = ScriptState {
        create: VecDeque::from([TransportOutcome::Ambiguous(AmbiguityReason::Timeout)]),
        status_ref: VecDeque::from([
            TransportOutcome::AuthStale { http_status: 401 },
            TransportOutcome::AuthStale { http_status: 401 },
            status_ok("GW-AUTH", "OPEN", 0),
        ]),
        ..Default::default()
    };
    let mut exec = live_exec(st);
    let out = exec
        .place_order(base_req(), DATE, NOW_MS, true)
        .await
        .unwrap();
    // Lands despite the auth-stall — the budget clock never advanced during it.
    assert_eq!(
        out,
        PlaceResult::ResolvedLanded {
            order_id: "GW-AUTH".to_owned()
        }
    );
    assert_eq!(exec.transport.status_ref_refs().len(), 3);
}

// ---------------------------------------------------------------------------
// P7/P9: cancel loses the race to a fill → position exists
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn cancel_ambiguous_lost_race_to_fill() {
    let st = ScriptState {
        create: VecDeque::from([mut_ok("GW-1")]),
        cancel: VecDeque::from([TransportOutcome::Ambiguous(AmbiguityReason::Timeout)]),
        detail: VecDeque::from([detail("COMPLETED", 50)]),
        ..Default::default()
    };
    let mut exec = live_exec(st);
    exec.place_order(base_req(), DATE, NOW_MS, true)
        .await
        .unwrap();
    let cancel = GrowwCancelOrderReq {
        groww_order_id: "GW-1".to_owned(),
        segment: GrowwSegment::Fno,
    };
    let out = exec.cancel_order(cancel, DATE, NOW_MS, true).await.unwrap();
    assert_eq!(
        out,
        MutationResult::CancelLostRace {
            filled_quantity: 50
        }
    );
}

// ---------------------------------------------------------------------------
// P50 / carry-note (a): modify on an unresolved place is refused
// ---------------------------------------------------------------------------

#[tokio::test]
async fn modify_on_unresolved_place_is_refused() {
    let mut exec = live_exec(ScriptState::default());
    exec.register_unresolved_place("GW-AMBIG");
    let m = GrowwModifyOrderReq {
        groww_order_id: "GW-AMBIG".to_owned(),
        segment: GrowwSegment::Fno,
        quantity: Some(10),
        price: None,
        trigger_price: None,
        order_type: None,
    };
    let err = exec.modify_order(m, DATE, NOW_MS, true).await.unwrap_err();
    assert!(matches!(
        err,
        ExecError::Oms(GrowwOmsError::MutationInFlight { .. })
    ));
}

#[tokio::test]
async fn modify_on_unknown_order_is_refused() {
    let mut exec = live_exec(ScriptState::default());
    let m = GrowwModifyOrderReq {
        groww_order_id: "NEVER-SEEN".to_owned(),
        segment: GrowwSegment::Fno,
        quantity: Some(10),
        price: None,
        trigger_price: None,
        order_type: None,
    };
    let err = exec.modify_order(m, DATE, NOW_MS, true).await.unwrap_err();
    assert!(matches!(
        err,
        ExecError::Oms(GrowwOmsError::OrderParkedUnknown { .. })
    ));
}

#[tokio::test]
async fn modify_on_partial_fill_is_refused_o11() {
    let st = ScriptState {
        create: VecDeque::from([mut_ok("GW-1")]),
        ..Default::default()
    };
    let mut exec = live_exec(st);
    exec.place_order(base_req(), DATE, NOW_MS, true)
        .await
        .unwrap();
    // Simulate a partial fill observation.
    exec.observe(
        "GW-1",
        &GrowwOrderStatus::Open,
        Some(10),
        ObservationSource::Live,
    );
    let m = GrowwModifyOrderReq {
        groww_order_id: "GW-1".to_owned(),
        segment: GrowwSegment::Fno,
        quantity: Some(20),
        price: None,
        trigger_price: None,
        order_type: None,
    };
    let err = exec.modify_order(m, DATE, NOW_MS, true).await.unwrap_err();
    assert!(matches!(
        err,
        ExecError::Oms(GrowwOmsError::ModifyOnPartialFillUnproven { .. })
    ));
}

// ---------------------------------------------------------------------------
// P27: mutation after close refused
// ---------------------------------------------------------------------------

#[tokio::test]
async fn place_after_close_is_refused() {
    let mut exec = live_exec(ScriptState::default());
    let err = exec
        .place_order(base_req(), DATE, NOW_MS, false)
        .await
        .unwrap_err();
    assert!(matches!(err, ExecError::MarketClosed));
}

// ---------------------------------------------------------------------------
// P29: EOD sweep classifies + force-terminalizes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn eod_sweep_classifies_open_and_partial() {
    let mut exec = live_exec(ScriptState::default());
    exec.adopt_order(
        "O-OPEN",
        GrowwOrderStatus::Open,
        0,
        PlaceProvenance::Settled,
    );
    exec.adopt_order(
        "O-PART",
        GrowwOrderStatus::Open,
        20,
        PlaceProvenance::Settled,
    );
    exec.adopt_order(
        "O-DONE",
        GrowwOrderStatus::Completed,
        50,
        PlaceProvenance::Settled,
    );
    let report = exec.eod_sweep();
    assert!(report.expired.contains(&"O-OPEN".to_owned()));
    assert!(
        report
            .partial_fill_positions
            .iter()
            .any(|(id, q)| id == "O-PART" && *q == 20)
    );
    assert!(!report.expired.contains(&"O-DONE".to_owned()));
    // Force-terminalized locally.
    assert!(is_terminal(&exec.tracked_order("O-OPEN").unwrap().status));
}

// ---------------------------------------------------------------------------
// Ladder exhaustion → Unresolved (Critical)
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn place_ambiguity_exhausts_to_unresolved() {
    // Empty status_ref queue ⇒ every poll returns the default Ambiguous ⇒
    // the ladder spends its budget and exhausts.
    let st = ScriptState {
        create: VecDeque::from([TransportOutcome::Ambiguous(AmbiguityReason::Timeout)]),
        ..Default::default()
    };
    let mut exec = live_exec(st);
    let out = exec
        .place_order(base_req(), DATE, NOW_MS, true)
        .await
        .unwrap();
    assert_eq!(out, PlaceResult::Unresolved);
}

// ---------------------------------------------------------------------------
// Gate: Live construction is const-gated (false today); mode mutator cfg(test)
// ---------------------------------------------------------------------------

#[test]
fn live_send_permitted_is_false_today() {
    assert!(
        !live_send_permitted(true),
        "GROWW_ORDER_LIVE_FIRE must gate live off"
    );
    assert!(!live_send_permitted(false));
}

#[test]
fn new_live_refuses_construction_today() {
    let ledger = open_ledger("gate");
    let err = GrowwOrderExecutor::new_live(NullTransport, SecretString::from("t"), cfg(), ledger)
        .err()
        .expect("live construction must be refused today");
    assert!(matches!(err, ExecError::LiveDisabledAtCompileTime));
}

// ---------------------------------------------------------------------------
// Carry-note (b): the ledger fsync path runs under spawn_blocking, so the
// executor drives cleanly on a CURRENT-THREAD runtime (a block_in_place would
// PANIC here — this is the behavioural proof no block_in_place is used).
// ---------------------------------------------------------------------------

#[test]
fn place_runs_on_current_thread_runtime_no_block_in_place() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread rt");
    rt.block_on(async {
        let mut exec = GrowwOrderExecutor::new_paper(open_ledger("ct"), cfg());
        let out = exec
            .place_order(base_req(), DATE, NOW_MS, true)
            .await
            .unwrap();
        assert!(matches!(out, PlaceResult::PaperAccepted { .. }));
    });
}

// ---------------------------------------------------------------------------
// Carry-note (c): the reference-id salt draws from a CSPRNG (uuid v4) — two
// draws differ with overwhelming probability (behavioural entropy check).
// ---------------------------------------------------------------------------

#[test]
fn reference_id_salt_is_high_entropy() {
    let salts: std::collections::HashSet<u32> = (0..64).map(|_| reference_id_salt()).collect();
    // A predictable/constant salt would collapse this set; a CSPRNG keeps it
    // (essentially) all-distinct.
    assert!(
        salts.len() >= 60,
        "salt entropy too low: {} distinct/64",
        salts.len()
    );
}

#[test]
fn ladder_delays_follow_the_2_5_10_30_60_then_60_schedule() {
    assert_eq!(ladder_delay_for_step(0), 2);
    assert_eq!(ladder_delay_for_step(1), 5);
    assert_eq!(ladder_delay_for_step(2), 10);
    assert_eq!(ladder_delay_for_step(3), 30);
    assert_eq!(ladder_delay_for_step(4), 60);
    assert_eq!(ladder_delay_for_step(5), 60);
    assert_eq!(ladder_delay_for_step(99), 60);
}

// ---------------------------------------------------------------------------
// Smart Orders (GTT/OCO) — executor integration (2026-07-16 directive)
// ---------------------------------------------------------------------------

use crate::oms::groww::smart_orders::{
    GrowwCreateOcoReq, GttOrderLeg, OcoStopLossLeg, OcoTargetLeg, SmartModifyBody,
    SmartModifyFields, SmartOrderCreate, SmartOrderError, SmartOrderGates, SmartOrderListPayload,
    SmartOrderListQuery, SmartOrderPayload, SmartOrderStatus, SmartOrderType, StopLossLegOrderType,
    TargetLegOrderType,
};

fn smart_gates_on() -> SmartOrderGates {
    SmartOrderGates {
        smart_orders_read: true,
        smart_orders_write: true,
        live_fire_requested: true, // inert without GROWW_ORDER_LIVE_FIRE
        max_order_quantity: 100,
        sibling_cancel_deadline_secs: 30,
        reconcile_poll_secs: 15,
    }
}

fn oco_create() -> SmartOrderCreate {
    SmartOrderCreate::Oco(GrowwCreateOcoReq {
        reference_id: "PLACEHOLDER0000".to_owned(),
        smart_order_type: SmartOrderType::Oco,
        segment: GrowwSegment::Fno,
        trading_symbol: "NIFTY26JUL28500CE".to_owned(),
        quantity: 75,
        net_position_quantity: 75,
        transaction_type: GrowwTransactionType::Sell,
        target: OcoTargetLeg {
            trigger_price: 12_200,
            order_type: TargetLegOrderType::Limit,
            price: Some(12_150),
        },
        stop_loss: OcoStopLossLeg {
            trigger_price: 9_800,
            order_type: StopLossLegOrderType::SlM,
            price: None,
        },
        product_type: GrowwProduct::Nrml,
        exchange: GrowwExchange::Nse,
        duration: GrowwValidity::Day,
    })
}

fn paper_smart_exec(tag: &str) -> GrowwOrderExecutor<NullTransport> {
    let mut exec = GrowwOrderExecutor::new_paper(open_ledger(tag), cfg());
    exec.set_smart_gates(smart_gates_on());
    exec
}

#[tokio::test]
async fn smart_place_paper_tracks_a_synthetic_id() {
    let mut exec = paper_smart_exec("sm-place");
    let out = exec
        .place_smart_order(oco_create(), DATE, NOW_MS, true)
        .await
        .unwrap();
    let PlaceResult::PaperAccepted { order_id } = out else {
        panic!("expected PaperAccepted, got {out:?}");
    };
    assert!(order_id.starts_with("PAPER-TV"));
    let book = exec.smart_book();
    let guard = book.lock().await;
    let t = guard.get(&order_id).expect("tracked");
    assert_eq!(t.status, SmartOrderStatus::Active);
    assert_eq!(t.smart_order_type, SmartOrderType::Oco);
    assert_eq!(t.quantity, 75);
}

#[tokio::test]
async fn smart_place_refused_when_write_gate_closed_or_market_closed() {
    // Default gates = disabled() — fail-closed without set_smart_gates.
    let mut exec = GrowwOrderExecutor::new_paper(open_ledger("sm-gate"), cfg());
    let out = exec
        .place_smart_order(oco_create(), DATE, NOW_MS, true)
        .await;
    assert!(matches!(
        out,
        Err(ExecError::Smart(SmartOrderError::WriteGateClosed))
    ));
    // Market closed refuses BEFORE the gate (session guard first).
    let mut exec = paper_smart_exec("sm-closed");
    let out = exec
        .place_smart_order(oco_create(), DATE, NOW_MS, false)
        .await;
    assert!(matches!(out, Err(ExecError::MarketClosed)));
}

#[tokio::test]
async fn smart_place_validation_uses_the_smart_gate_quantity_cap() {
    let mut exec = GrowwOrderExecutor::new_paper(open_ledger("sm-qty"), cfg());
    let mut gates = smart_gates_on();
    gates.max_order_quantity = 10; // below the 75 requested
    exec.set_smart_gates(gates);
    let out = exec
        .place_smart_order(oco_create(), DATE, NOW_MS, true)
        .await;
    assert!(matches!(
        out,
        Err(ExecError::Smart(SmartOrderError::QuantityRefused {
            quantity: 75,
            max: 10
        }))
    ));
}

#[tokio::test]
async fn smart_place_refused_at_the_tracked_open_cap() {
    let mut exec = paper_smart_exec("sm-cap");
    let cap = tickvault_common::constants::GROWW_ORDER_MAX_TRACKED_OPEN_ORDERS;
    for _ in 0..cap {
        exec.place_smart_order(oco_create(), DATE, NOW_MS, true)
            .await
            .unwrap();
    }
    let out = exec
        .place_smart_order(oco_create(), DATE, NOW_MS, true)
        .await;
    assert!(matches!(
        out,
        Err(ExecError::Smart(SmartOrderError::TrackedCapExceeded { .. }))
    ));
}

#[tokio::test]
async fn smart_modify_paper_applies_the_matrix_then_updates_the_book() {
    let mut exec = paper_smart_exec("sm-mod");
    let PlaceResult::PaperAccepted { order_id } = exec
        .place_smart_order(oco_create(), DATE, NOW_MS, true)
        .await
        .unwrap()
    else {
        panic!("paper place");
    };
    // An OCO-immutable field is the typed GROWW-OCO-04 refusal, pre-HTTP.
    let out = exec
        .modify_smart_order(
            &order_id,
            SmartModifyFields {
                trigger_price: Some(1),
                ..Default::default()
            },
            DATE,
            NOW_MS,
            true,
        )
        .await;
    assert!(matches!(
        out,
        Err(ExecError::Smart(SmartOrderError::ImmutableField {
            smart_order_type: "OCO",
            ..
        }))
    ));
    // A legal OCO modify (quantity) is applied to the tracked book.
    let out = exec
        .modify_smart_order(
            &order_id,
            SmartModifyFields {
                quantity: Some(50),
                ..Default::default()
            },
            DATE,
            NOW_MS,
            true,
        )
        .await
        .unwrap();
    assert_eq!(out, MutationResult::Accepted);
    let book = exec.smart_book();
    let guard = book.lock().await;
    let t = guard.get(&order_id).expect("tracked");
    assert_eq!(t.quantity, 50);
    assert_eq!(t.modifications, 1);
}

#[tokio::test]
async fn smart_mutations_refuse_unknown_terminal_and_implausible_ids() {
    let mut exec = paper_smart_exec("sm-elig");
    let out = exec
        .cancel_smart_order("gtt_missing", DATE, NOW_MS, true)
        .await;
    assert!(matches!(
        out,
        Err(ExecError::Smart(SmartOrderError::UnknownSmartOrder { .. }))
    ));
    let out = exec
        .modify_smart_order(
            "../etc",
            SmartModifyFields {
                quantity: Some(1),
                ..Default::default()
            },
            DATE,
            NOW_MS,
            true,
        )
        .await;
    assert!(matches!(
        out,
        Err(ExecError::Smart(SmartOrderError::ImplausibleSmartOrderId))
    ));
    // Cancel then cancel again — the second is AlreadyTerminal.
    let PlaceResult::PaperAccepted { order_id } = exec
        .place_smart_order(oco_create(), DATE, NOW_MS, true)
        .await
        .unwrap()
    else {
        panic!("paper place");
    };
    let out = exec
        .cancel_smart_order(&order_id, DATE, NOW_MS, true)
        .await
        .unwrap();
    assert_eq!(out, MutationResult::Accepted);
    {
        let book = exec.smart_book();
        let guard = book.lock().await;
        assert_eq!(
            guard.get(&order_id).expect("tracked").status,
            SmartOrderStatus::Cancelled
        );
    }
    let out = exec.cancel_smart_order(&order_id, DATE, NOW_MS, true).await;
    assert!(matches!(
        out,
        Err(ExecError::Smart(SmartOrderError::AlreadyTerminal { .. }))
    ));
}

#[tokio::test]
async fn smart_reconcile_is_read_gated_and_paper_is_a_no_op() {
    // Read gate closed (gates default-disabled).
    let mut exec = GrowwOrderExecutor::new_paper(open_ledger("sm-rec-gate"), cfg());
    let out = exec
        .reconcile_smart_orders(NOW_MS, true, &HashMap::new())
        .await;
    assert!(matches!(
        out,
        Err(ExecError::Smart(SmartOrderError::ReadGateClosed))
    ));
    // Outside market hours refuses even with the gate on.
    let mut exec = paper_smart_exec("sm-rec-hours");
    let out = exec
        .reconcile_smart_orders(NOW_MS, false, &HashMap::new())
        .await;
    assert!(matches!(
        out,
        Err(ExecError::Smart(SmartOrderError::ReadGateClosed))
    ));
    // The paper lane returns an empty report (PAPER- ids have no broker side).
    let out = exec
        .reconcile_smart_orders(NOW_MS, true, &HashMap::new())
        .await
        .unwrap();
    assert_eq!(out.polled, 0);
    assert!(out.findings.is_empty());
}

// -- live-lane smart paths against the scripted transport --------------------

#[derive(Default)]
struct SmartScriptState {
    create: VecDeque<TransportOutcome<SmartOrderPayload>>,
    modify: VecDeque<TransportOutcome<SmartOrderPayload>>,
    cancel: VecDeque<TransportOutcome<SmartOrderPayload>>,
    get: VecDeque<TransportOutcome<SmartOrderPayload>>,
}

#[derive(Default)]
struct SmartScriptedTransport {
    st: StdMutex<SmartScriptState>,
    create_refs: StdMutex<Vec<String>>,
}

impl OrderTransport for SmartScriptedTransport {
    async fn create_order(
        &self,
        _req: &GrowwCreateOrderReq,
        _token: &SecretString,
        _receipt: &IntentReceipt,
    ) -> TransportOutcome<GrowwMutationRespPayload> {
        TransportOutcome::Ambiguous(AmbiguityReason::Timeout)
    }
    async fn modify_order(
        &self,
        _req: &GrowwModifyOrderReq,
        _token: &SecretString,
        _receipt: &IntentReceipt,
    ) -> TransportOutcome<GrowwMutationRespPayload> {
        TransportOutcome::Ambiguous(AmbiguityReason::Timeout)
    }
    async fn cancel_order(
        &self,
        _req: &GrowwCancelOrderReq,
        _token: &SecretString,
        _receipt: &IntentReceipt,
    ) -> TransportOutcome<GrowwMutationRespPayload> {
        TransportOutcome::Ambiguous(AmbiguityReason::Timeout)
    }
    async fn get_status_by_id(
        &self,
        _id: &str,
        _segment: GrowwSegment,
        _token: &SecretString,
    ) -> TransportOutcome<GrowwOrderStatusPayload> {
        TransportOutcome::Ambiguous(AmbiguityReason::Timeout)
    }
    async fn get_status_by_reference(
        &self,
        _r: &str,
        _segment: GrowwSegment,
        _token: &SecretString,
    ) -> TransportOutcome<GrowwOrderStatusPayload> {
        TransportOutcome::Ambiguous(AmbiguityReason::Timeout)
    }
    async fn get_order_detail(
        &self,
        _id: &str,
        _segment: GrowwSegment,
        _token: &SecretString,
    ) -> TransportOutcome<GrowwOrderDetailPayload> {
        TransportOutcome::Ambiguous(AmbiguityReason::Timeout)
    }
    async fn list_orders(
        &self,
        _segment: GrowwSegment,
        _page: u32,
        _page_size: u32,
        _token: &SecretString,
    ) -> TransportOutcome<Vec<GrowwOrderDetailPayload>> {
        TransportOutcome::Success(Vec::new())
    }
    async fn get_trades(
        &self,
        _id: &str,
        _segment: GrowwSegment,
        _page: u32,
        _page_size: u32,
        _token: &SecretString,
    ) -> TransportOutcome<Vec<GrowwTradeRow>> {
        TransportOutcome::Success(Vec::new())
    }
}

impl crate::oms::groww::api_client::SmartOrderTransport for SmartScriptedTransport {
    async fn create_smart_order(
        &self,
        req: &SmartOrderCreate,
        _token: &SecretString,
        _receipt: &IntentReceipt,
    ) -> TransportOutcome<SmartOrderPayload> {
        self.create_refs
            .lock()
            .unwrap()
            .push(req.reference_id().to_owned());
        self.st
            .lock()
            .unwrap()
            .create
            .pop_front()
            .unwrap_or(TransportOutcome::Ambiguous(AmbiguityReason::Timeout))
    }
    async fn modify_smart_order(
        &self,
        _smart_order_id: &str,
        _body: &SmartModifyBody,
        _token: &SecretString,
        _receipt: &IntentReceipt,
    ) -> TransportOutcome<SmartOrderPayload> {
        self.st
            .lock()
            .unwrap()
            .modify
            .pop_front()
            .unwrap_or(TransportOutcome::Ambiguous(AmbiguityReason::Timeout))
    }
    async fn cancel_smart_order(
        &self,
        _segment: GrowwSegment,
        _smart_order_type: SmartOrderType,
        _smart_order_id: &str,
        _token: &SecretString,
        _receipt: &IntentReceipt,
    ) -> TransportOutcome<SmartOrderPayload> {
        self.st
            .lock()
            .unwrap()
            .cancel
            .pop_front()
            .unwrap_or(TransportOutcome::Ambiguous(AmbiguityReason::Timeout))
    }
    async fn get_smart_order(
        &self,
        _segment: GrowwSegment,
        _smart_order_type: SmartOrderType,
        _smart_order_id: &str,
        _token: &SecretString,
    ) -> TransportOutcome<SmartOrderPayload> {
        self.st
            .lock()
            .unwrap()
            .get
            .pop_front()
            .unwrap_or(TransportOutcome::Ambiguous(AmbiguityReason::Timeout))
    }
    async fn list_smart_orders(
        &self,
        _query: &SmartOrderListQuery,
        _token: &SecretString,
    ) -> TransportOutcome<SmartOrderListPayload> {
        TransportOutcome::Ambiguous(AmbiguityReason::Timeout)
    }
}

fn smart_payload(id: &str, status: &str) -> TransportOutcome<SmartOrderPayload> {
    let json = format!(r#"{{"smart_order_id":"{id}","status":"{status}"}}"#);
    TransportOutcome::Success(serde_json::from_str(&json).expect("payload"))
}

fn live_smart_exec(st: SmartScriptState, tag: &str) -> GrowwOrderExecutor<SmartScriptedTransport> {
    let mut exec = GrowwOrderExecutor::new_live_for_test(
        SmartScriptedTransport {
            st: StdMutex::new(st),
            create_refs: StdMutex::new(Vec::new()),
        },
        SecretString::from("test-token"),
        cfg(),
        open_ledger(tag),
    );
    exec.set_smart_gates(smart_gates_on());
    exec
}

#[tokio::test]
async fn smart_live_place_success_adopts_the_broker_id() {
    let st = SmartScriptState {
        create: VecDeque::from([smart_payload("oco_live1", "ACTIVE")]),
        ..Default::default()
    };
    let mut exec = live_smart_exec(st, "sm-live-ok");
    let out = exec
        .place_smart_order(oco_create(), DATE, NOW_MS, true)
        .await
        .unwrap();
    assert_eq!(
        out,
        PlaceResult::Accepted {
            order_id: "oco_live1".to_owned()
        }
    );
    let book = exec.smart_book();
    assert!(book.lock().await.contains("oco_live1"));
    // The executor overwrote the reference id with a fresh TV… id.
    let refs = exec.transport.create_refs.lock().unwrap().clone();
    assert_eq!(refs.len(), 1);
    assert!(refs[0].starts_with("TV"));
}

#[tokio::test]
async fn smart_live_place_reject_and_ambiguity_fail_closed() {
    // Definitive reject → Rejected with the GA code.
    let st = SmartScriptState {
        create: VecDeque::from([TransportOutcome::Rejected {
            http_status: 400,
            ga_code: Some("GA001".to_owned()),
            message: Some("bad".to_owned()),
        }]),
        ..Default::default()
    };
    let mut exec = live_smart_exec(st, "sm-live-rej");
    let out = exec
        .place_smart_order(oco_create(), DATE, NOW_MS, true)
        .await
        .unwrap();
    assert_eq!(
        out,
        PlaceResult::Rejected {
            ga_code: Some("GA001".to_owned())
        }
    );
    // Ambiguity is UNRESOLVABLE (no by-reference lookup on this surface):
    // fail-closed Unresolved, no retry, no adoption.
    let st = SmartScriptState::default(); // empty queue -> Timeout ambiguous
    let mut exec = live_smart_exec(st, "sm-live-amb");
    let out = exec
        .place_smart_order(oco_create(), DATE, NOW_MS, true)
        .await
        .unwrap();
    assert_eq!(out, PlaceResult::Unresolved);
    assert_eq!(
        exec.transport.create_refs.lock().unwrap().len(),
        1,
        "never re-sent (the double-send class)"
    );
    let book = exec.smart_book();
    assert_eq!(book.lock().await.open_count(), 0);
}

#[tokio::test]
async fn smart_live_cancel_adopts_the_cancelled_echo() {
    let st = SmartScriptState {
        create: VecDeque::from([smart_payload("gtt_live1", "ACTIVE")]),
        cancel: VecDeque::from([smart_payload("gtt_live1", "CANCELLED")]),
        ..Default::default()
    };
    let mut exec = live_smart_exec(st, "sm-live-can");
    let gtt = SmartOrderCreate::Gtt(crate::oms::groww::smart_orders::GrowwCreateGttReq {
        reference_id: "PLACEHOLDER0000".to_owned(),
        smart_order_type: SmartOrderType::Gtt,
        segment: GrowwSegment::Cash,
        trading_symbol: "RELIANCE".to_owned(),
        quantity: 10,
        trigger_price: 398_500,
        trigger_direction: crate::oms::groww::smart_orders::TriggerDirection::Up,
        order: GttOrderLeg {
            order_type: GrowwOrderType::Market,
            price: None,
            transaction_type: GrowwTransactionType::Sell,
        },
        child_legs: None,
        product_type: GrowwProduct::Cnc,
        exchange: GrowwExchange::Nse,
        duration: GrowwValidity::Day,
    });
    exec.place_smart_order(gtt, DATE, NOW_MS, true)
        .await
        .unwrap();
    let out = exec
        .cancel_smart_order("gtt_live1", DATE, NOW_MS, true)
        .await
        .unwrap();
    assert_eq!(out, MutationResult::Accepted);
    let book = exec.smart_book();
    assert_eq!(
        book.lock().await.get("gtt_live1").expect("tracked").status,
        SmartOrderStatus::Cancelled
    );
}

// -- adversarial round 1 fixes ------------------------------------------------

#[tokio::test]
async fn smart_live_place_implausible_ack_id_is_unresolved_never_adopted() {
    // Finding 3: a 2xx SUCCESS echoing an IMPLAUSIBLE broker id must never
    // enter the book (it would later be spliced into a URL path).
    let st = SmartScriptState {
        create: VecDeque::from([smart_payload("..%2Fevil", "ACTIVE")]),
        ..Default::default()
    };
    let mut exec = live_smart_exec(st, "sm-live-badid");
    let out = exec
        .place_smart_order(oco_create(), DATE, NOW_MS, true)
        .await
        .unwrap();
    assert_eq!(out, PlaceResult::Unresolved);
    let book = exec.smart_book();
    assert_eq!(book.lock().await.open_count(), 0, "implausible id never adopted");
}

#[tokio::test]
async fn smart_modify_already_terminal_carries_the_raw_status() {
    // Finding 7: the AlreadyTerminal refusal names the REAL terminal status
    // (FAILED here), never a mislabeled catch-all.
    let st = SmartScriptState {
        create: VecDeque::from([smart_payload("oco_term1", "ACTIVE")]),
        ..Default::default()
    };
    let mut exec = live_smart_exec(st, "sm-live-term");
    exec.place_smart_order(oco_create(), DATE, NOW_MS, true)
        .await
        .unwrap();
    {
        let book = exec.smart_book();
        book.lock().await.get_mut("oco_term1").expect("tracked").status =
            SmartOrderStatus::Failed;
    }
    let err = exec
        .modify_smart_order(
            "oco_term1",
            SmartModifyFields {
                quantity: Some(25),
                ..Default::default()
            },
            DATE,
            NOW_MS,
            true,
        )
        .await
        .unwrap_err();
    match err {
        ExecError::Smart(SmartOrderError::AlreadyTerminal { status, .. }) => {
            assert_eq!(status, "FAILED", "raw terminal label, never COMPLETED");
        }
        other => panic!("expected AlreadyTerminal, got {other:?}"),
    }
}

#[tokio::test]
async fn smart_live_cancel_backward_echo_is_parked_not_adopted() {
    // Finding 8: a cancel-success echo carrying a BACKWARD status must be
    // parked by the FSM, never blindly assigned.
    let st = SmartScriptState {
        create: VecDeque::from([smart_payload("oco_back1", "ACTIVE")]),
        cancel: VecDeque::from([smart_payload("oco_back1", "ACTIVE")]),
        ..Default::default()
    };
    let mut exec = live_smart_exec(st, "sm-live-back");
    exec.place_smart_order(oco_create(), DATE, NOW_MS, true)
        .await
        .unwrap();
    {
        let book = exec.smart_book();
        book.lock().await.get_mut("oco_back1").expect("tracked").status =
            SmartOrderStatus::Triggered;
    }
    let out = exec
        .cancel_smart_order("oco_back1", DATE, NOW_MS, true)
        .await
        .unwrap();
    assert_eq!(out, MutationResult::Accepted);
    let book = exec.smart_book();
    assert_eq!(
        book.lock().await.get("oco_back1").expect("tracked").status,
        SmartOrderStatus::Triggered,
        "backward echo parked — reconcile owns the divergence"
    );
}

#[tokio::test]
async fn smart_budget_denial_terminalizes_the_intent() {
    // Finding 10: a budget-denied smart mutation must NOT leave a
    // forever-open Recorded intent (it would block the order's
    // mutation-in-flight index until restart).
    let dir = temp_ledger_dir("sm-budget");
    let ledger = IntentLedger::open(&dir, DATE).expect("ledger open");
    let mut exec = GrowwOrderExecutor::new_live_for_test(
        SmartScriptedTransport {
            st: StdMutex::new(SmartScriptState {
                // Enough scripted ACKs for every send that passes the budget.
                create: VecDeque::from(
                    (0..12)
                        .map(|i| smart_payload(&format!("oco_b{i}"), "ACTIVE"))
                        .collect::<Vec<_>>(),
                ),
                ..Default::default()
            }),
            create_refs: StdMutex::new(Vec::new()),
        },
        SecretString::from("test-token"),
        cfg(),
        ledger,
    );
    exec.set_smart_gates(smart_gates_on());
    let mut denied = false;
    for _ in 0..12 {
        match exec.place_smart_order(oco_create(), DATE, NOW_MS, true).await {
            Err(ExecError::RateBudgetExceeded(_)) => {
                denied = true;
                break;
            }
            Ok(_) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert!(denied, "the 5/sec mutation self-cap must deny within 12 rapid places");
    drop(exec);
    // Replay the ledger file: every SmartPlace intent must be TERMINAL —
    // the denied one as resolved_not_landed, the sent ones as acked.
    let file = std::fs::read_dir(&dir)
        .expect("dir")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "ndjson"))
        .expect("ledger file");
    let buf = std::fs::read(&file).expect("read ledger");
    let replay = crate::oms::groww::intent_ledger::replay_bytes(&buf).expect("replay");
    let mut not_landed = 0_usize;
    for s in replay.intents.values() {
        assert_eq!(s.kind, IntentKind::SmartPlace);
        assert!(
            s.last_phase.is_terminal(),
            "no forever-open intent after budget denial (got {:?})",
            s.last_phase
        );
        if s.last_phase == IntentPhase::ResolvedNotLanded {
            not_landed += 1;
        }
    }
    assert_eq!(not_landed, 1, "exactly the denied place is resolved_not_landed");
    let _ = std::fs::remove_dir_all(&dir);
}

// -- adversarial round 2, HIGH: reconcile GET id-mismatch is never adopted ----

#[tokio::test]
async fn smart_reconcile_foreign_id_echo_never_terminalizes_tracked_order() {
    // A status GET response carrying a DIFFERENT smart_order_id than the one
    // requested must be counted/degraded and dropped — never classified or
    // applied (a foreign COMPLETED echo would silently terminalize a live OCO
    // and silence the OCO-02 double-fill guard).
    let st = SmartScriptState {
        create: VecDeque::from([smart_payload("oco_hi1", "TRIGGERED")]),
        // The GET returns a FOREIGN id with a terminal status.
        get: VecDeque::from([smart_payload("oco_FOREIGN", "COMPLETED")]),
        ..Default::default()
    };
    let mut exec = live_smart_exec(st, "sm-reconcile-idmm");
    exec.place_smart_order(oco_create(), DATE, NOW_MS, true)
        .await
        .unwrap();
    // Arm the episode: the tracked order is TRIGGERED (live, non-terminal).
    {
        let book = exec.smart_book();
        let mut g = book.lock().await;
        let t = g.get_mut("oco_hi1").expect("tracked");
        t.status = SmartOrderStatus::Triggered;
        t.triggered_at_ms = Some(NOW_MS);
    }
    let report = exec
        .reconcile_smart_orders(NOW_MS + 1_000, true, &HashMap::new())
        .await
        .unwrap();
    // Degraded with id_mismatch; the foreign payload never became a finding.
    assert!(
        report.degraded.contains(&"id_mismatch"),
        "id mismatch must degrade the pass: {report:?}"
    );
    assert!(
        report.findings.is_empty(),
        "a mismatched payload must never be classified: {report:?}"
    );
    // The tracked order stays TRIGGERED (live) — NOT terminalized.
    let book = exec.smart_book();
    let g = book.lock().await;
    let t = g.get("oco_hi1").expect("still tracked");
    assert_eq!(
        t.status,
        SmartOrderStatus::Triggered,
        "foreign COMPLETED echo must NOT terminalize the live OCO"
    );
}
