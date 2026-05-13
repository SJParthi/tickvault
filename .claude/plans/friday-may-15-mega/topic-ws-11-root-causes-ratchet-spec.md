# Topic — The 11 Root Causes of "Connected ≠ Streaming" — Ratchet Test Spec

> **Status:** DRAFT (discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** `topic-ws-flow-health-7-layer-defense.md` > this file.
> **Scope:** Pin each of the 11 root causes as its own ratchet test for future implementation.
> **Purpose:** Discussion + argument before code is written. Each scenario gets a "how do we simulate this in a test" answer.

---

## 🚗 Auto-Driver Story

> Sir, yesterday I named 11 different ways the phone-line can SAY it's connected while no one's talking. Today we ask: for EACH of those 11 ways, how do we PROVE in a test that our 7 bodyguards catch it? Like asking the PM's security team "show me you've practiced the scenario where a sniper hides on a rooftop, a scenario where a fake ambulance approaches, a scenario where a drone drops something..." — each scenario needs a drill.

---

## 📋 The 11 root causes mapped to test simulation

| # | Root cause | How to simulate in test | Expected layer fires | Pass criteria |
|---|---|---|---|---|
| **1** | Subscribe-replay lost on reconnect | `SubscribeRxGuard::drop()` mid-cycle; observe reconnect; assert no subscribe message sent | L1 (no frames) → L2 (ACK timeout) → resub | First frame within 5s of L2 retry |
| **2** | Token refresh DURING reconnect | Mock token refresh to fire at handshake +500ms; old JWT becomes invalid | L4 PREVENT (pre-reconnect age check) | Pre-refresh fires; new JWT used; handshake succeeds |
| **3** | Dhan-side stale session ID | Mock WS server holds old subscription state, drops incoming subs silently | L3 RECONCILE | Daily reconcile detects drift; resub fired |
| **4** | Conn capacity overflow (>5/feed) | Open 6 WS conns in test harness; observe Dhan silently kills oldest | L7 COOLDOWN (30s drain) | After cooldown, conn-count = 5; new conn succeeds |
| **5** | Binary frames blocked (proxy/firewall) | Mock WS that accepts handshake, sends pings, drops binary frames | L1 (zero frames despite Connected) | WS-FLOW-01 fires within 35s |
| **6** | Persist backpressure deadlock | Force QuestDB ILP latency to 5s; observe tick channel fill; read loop stalls | L1 + channel-depth gauge alarm | Both alarms fire; backpressure release within 60s |
| **7** | Stale SIDs after instrument refresh | Mid-day mock instrument master swap; old SIDs become invalid | L3 RECONCILE | Drift detected; client-side state synced |
| **8** | TCP keepalive vs Dhan ping race | Set kernel keepalive < Dhan ping interval; observe socket close before Dhan timeout | L5 ping audit | Anomaly counter increments; kernel keepalive disabled |
| **9** | Token expired mid-handshake | Mock token TTL to 100ms; reconnect at TTL+50ms | L4 pre-reconnect age check | Force-refresh fires; handshake succeeds |
| **10** | Subscribe sent before Dhan auth complete | Send Subscribe in same TCP frame as Upgrade response | L2 ACK timeout → retry | First frame within 5s of L2 retry |
| **11** | Phantom slot (server/client divergence) | Mock server forgets one of N subscribed SIDs randomly | L3 RECONCILE | Drift detected per missing SID; resub fired |

---

## 🔬 Per-scenario test fixture design

### Scenario 1 — Subscribe-replay lost

```rust
#[tokio::test]
async fn scenario_01_subscribe_replay_lost() {
    let mock = MockDhanWsServer::new()
        .with_handshake_ok()
        .with_first_subscribe_dropped()  // Server "accepts" but doesn't process
        .build();

    let conn = WsConnection::connect(mock.url()).await.unwrap();
    conn.subscribe(vec![SID(13)]).await.unwrap();

    // L1 should detect no frames in 5s
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(conn.frame_rate_5s(), 0);

    // L2 should fire ACK timeout
    let ack_outcome = conn.wait_for_subscribe_ack(Duration::from_secs(5)).await;
    assert_matches!(ack_outcome, Err(AckTimeout));

    // L2 should retry once
    let retry_outcome = conn.subscribe_with_retry(vec![SID(13)]).await;
    assert!(retry_outcome.is_ok());

    // Within 5s of retry, first frame should arrive
    let first_frame = conn.wait_first_frame(Duration::from_secs(5)).await;
    assert!(first_frame.is_ok());
}
```

**Open question:** Mock server complexity. Do we build a full `MockDhanWsServer` or use `tokio-tungstenite`'s test utilities?

### Scenario 2 — Token refresh during reconnect

```rust
#[tokio::test]
async fn scenario_02_token_refresh_during_reconnect() {
    let token_manager = MockTokenManager::with_expiry(Duration::from_millis(500));
    let mock = MockDhanWsServer::new()
        .with_token_validation()  // Server checks JWT signature
        .build();

    // Sleep 600ms — token expires
    tokio::time::sleep(Duration::from_millis(600)).await;

    // Trigger reconnect WITH old token
    let result = WsConnection::reconnect_with_l4_guard(&token_manager, mock.url()).await;

    // L4 should detect age > threshold, force-refresh, then handshake with NEW token
    assert!(result.is_ok());
    assert_eq!(token_manager.force_refresh_count(), 1);
}
```

### Scenario 3 — Dhan-side stale session ID

```rust
#[tokio::test]
async fn scenario_03_dhan_stale_session_id() {
    let mock = MockDhanWsServer::new()
        .with_session_id_drift(SID(13))  // Server "forgets" SID 13 silently
        .build();

    let conn = WsConnection::connect(mock.url()).await.unwrap();
    conn.subscribe(vec![SID(13)]).await.unwrap();

    // L1 detects zero frames
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(conn.frame_rate_5s(), 0);

    // L3 daily reconcile runs (mocked to fire on demand for test)
    let reconcile_outcome = run_daily_reconcile(&conn).await;
    assert_eq!(reconcile_outcome.drift_detected, 1);

    // Reconcile re-subscribes; frames flow
    let first_frame = conn.wait_first_frame(Duration::from_secs(5)).await;
    assert!(first_frame.is_ok());
}
```

### Scenarios 4–11 — Similar pattern (deferred details)

Each follows the template:
1. Build mock that simulates the specific failure
2. Trigger the failure
3. Assert the expected layer fires within time budget
4. Assert recovery completes within time budget

---

## 🚨 What COULD go wrong with these tests

### Risk 1 — Mock divergence

If `MockDhanWsServer` doesn't match real Dhan behavior, tests pass but production fails. We've been bitten before (Ticket #5519522 / `wss://full-depth-api.dhan.co/twohundreddepth` vs `/`).

**Mitigation:** Calibrate mock against `dhanhq` Python SDK behavior + recorded traffic captures.

### Risk 2 — Race conditions in mock timing

`tokio::time::sleep` in tests is not deterministic across CI runners. A test that passes locally may flake in CI.

**Mitigation:** Use `tokio::time::pause()` + `advance()` for deterministic time control. NO real sleeps in tests.

### Risk 3 — Test pollution between scenarios

If scenario N leaves state in shared globals (e.g., metrics registry), scenario N+1 reads stale state.

**Mitigation:** Each test uses an isolated metrics registry. `loom`-style test isolation.

### Risk 4 — Coverage gaps

Even with 11 scenarios, real Dhan can hit a 12th unknown failure mode.

**Mitigation:** Add `scenario_12_chaos_random` that fuzzes Dhan behavior. If it ever finds a new failure mode, promote it to a named scenario.

---

## 🎯 Discussion items

### D1 — Do we want each scenario to be its OWN test file, or one big file?

**Argument for separate files:** Clear failure attribution. `cargo test scenario_05` runs exactly one test.

**Argument for one file:** Shared fixtures + mock setup avoid duplication. ~600 LoC total vs 1200 spread.

**My vote:** ONE file `crates/core/tests/ws_flow_11_scenarios.rs`, each scenario as `#[tokio::test]` fn. Shared `MockDhanWsServer` builder.

### D2 — Should we replay REAL Dhan traffic in tests?

We have `data/captures/` directory in some test fixtures. Could record real Dhan WS traffic + replay.

**Argument for:** Most realistic. Catches mock-vs-reality divergence.

**Argument against:** Captures contain tokens / client IDs / IP addresses. PII concern. Replay drift over time.

**My vote:** NO. Sanitized synthetic mocks only. Real captures stay in private operator notes.

### D3 — Boot smoke test vs CI test

Plan §16 says "boot smoke test runs once per process at 09:13:30 IST". CI tests run in cargo test.

**Same code? Different code? Subset?**

**Proposal:** ONE test suite (`ws_flow_11_scenarios.rs`) runs in BOTH CI (full battery) AND production boot (subset = scenarios 1, 5, 6, 9, 11 — fastest). Production boot timing constraint: must complete < 30 sec.

### D4 — Chaos test integration

Plan §16 says "chaos_ws_soft_restart_tick_loss.rs". Where does this live?

**Proposal:** `crates/storage/tests/chaos_ws_soft_restart_tick_loss.rs` — alongside existing 16 chaos tests. Total chaos suite goes from 16 → 17.

### D5 — How long is acceptable for L6 soft restart?

Plan says ~80s worst case. Operator's charter says "zero ticks lost". Math:
- Peak ingest: 60,000 ticks/sec
- 80s × 60,000 = 4.8M ticks
- Rescue ring: 5M ticks
- Margin: 200K ticks (4% headroom)

**Verdict:** Inside envelope BUT tight. Need ratchet test that runs at PEAK tick rate, not average.

**Counter:** Real-world peak is closer to 10-15K ticks/sec sustained. 60K is theoretical max. 80s × 15K = 1.2M ticks — comfortable margin.

**Open question:** Should the soft-restart be bounded harder (e.g., 30s max)? Trade-off: faster restart vs less time for Dhan-side conn counter to drain.

---

## 🎤 The 11 + 1 question

What's scenario #12? The one we haven't named yet. Operator should brainstorm:

| Possible #12 | Description |
|---|---|
| (a) Subscribe message TOO BIG (>16KB) silently truncated | If a single Subscribe message exceeds Dhan's frame size limit, do they reject or accept-then-drop? |
| (b) Quota exceeded mid-day (Data API 100K/day limit) | Dhan starts silently rejecting Subscribe messages |
| (c) Account suspended mid-session | Dhan disconnects all WS + sets `dataPlan` to expired |
| (d) Clock skew between app and Dhan | LTT timestamps go backwards; routing fails |
| (e) Memory pressure on app side | OS swap kicks in; tokio scheduler delays; pings missed |
| (f) BSE-side latency divergence | NSE_FNO ticks flow, BSE_FNO silent; one segment subscribed but no frames |

Pick which one(s) to add as scenario #12, #13, etc.

---

## 🎯 Open for argument

Floor's yours. Pick D1-D5 or surface a new angle.
