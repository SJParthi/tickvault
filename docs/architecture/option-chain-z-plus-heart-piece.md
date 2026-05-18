# Option Chain REST Loop — Z+ "Heart Piece" Design

> **Status:** DESIGN (no code shipped). Z+ 7-layer doctrine applied per `z-plus-defense-doctrine.md`.
> **Authority:** CLAUDE.md > operator-charter-forever.md > this file > defaults.
> **Created:** 2026-05-18 in response to operator demand: *"this one is extremely mandatory like a heart piece since this is nowhere tolerable dude... if this fails then we will never know right dude what is the current or last LTP and even all other greeks values also right dude... entire request and response we will miss it right."*
> **Companion:** `aws-indices-only-locked-architecture.md` §7, `.claude/rules/dhan/option-chain.md`.

---

## §0. Auto-driver one-liner (insta-reel)

> "Sir, imagine your shop's price board for 3 most important items (Nifty options / BankNifty options / Sensex options). Every 50 seconds the shop assistant runs to the wholesale market and copies down 600+ option prices + greeks for these 3 items. If the assistant gets stuck in traffic or the wholesale market is closed, the price board goes stale. **Rule: if the board is older than 60 seconds, STOP TRADING immediately.** Better to not trade than to trade on yesterday's prices. We also keep 5 backup phones to call the wholesale market AND record every single trip in 2 different notebooks so we can replay later if needed."

```
                09:15:30 IST market opens
                       │
                       ▼
   ┌──────────────────────────────────────────────────────┐
   │ Every 50 seconds, fire 3 CONCURRENT REST calls       │
   │ (Dhan: 1 req/3s per unique underlying — concurrent   │
   │  across underlyings allowed)                          │
   └──────────────────────────────────────────────────────┘
              │              │              │
              ▼              ▼              ▼
      NIFTY-13      BANKNIFTY-25      SENSEX-51
       (current      (current          (current
        expiry)       expiry)           expiry)
              │              │              │
              └──────────────┼──────────────┘
                             ▼
                    ┌────────────────┐
                    │ RAM cache      │   ← strategy reads this (NO DB)
                    │ per underlying │
                    └───────┬────────┘
                            │
                  async flush every cycle
                            ▼
              ┌────────────────────────────┐
              │ dhan_option_chain_raw      │   ← replay / audit
              │ option_chain_snapshots     │
              └────────────────────────────┘
```

---

## §1. Why this is the heart piece

| Decision the strategy makes | Data needed from option chain |
|---|---|
| "Is NIFTY 25600 CE worth buying NOW?" | last_price (LTP), top_bid_price, top_ask_price |
| "What's the implied volatility regime?" | implied_volatility for ATM CE+PE |
| "How is theta decay playing out today?" | greeks.theta, greeks.delta over time |
| "Has institutional positioning shifted?" | oi, volume, previous_oi |
| "Should we exit on a delta hedge violation?" | greeks.delta of held positions |
| "Is PCR signal valid?" | oi of every CE+PE strike |

**Without a fresh option chain, every one of these answers becomes UNKNOWN.** The strategy must fail-closed (not trade) — but the operator must know IMMEDIATELY that we're in fail-closed state. That's the heart-piece demand.

---

## §2. The Dhan REST contract (per `dhan/option-chain.md`)

| Element | Value |
|---|---|
| Endpoint | `POST https://api.dhan.co/v2/optionchain` |
| Required headers | `access-token: <JWT>` + `client-id: <ID>` |
| Body | `{ "UnderlyingScrip": int, "UnderlyingSeg": "IDX_I", "Expiry": "YYYY-MM-DD" }` |
| Rate limit | **1 unique request per 3 seconds** (per UnderlyingScrip+Expiry pair) |
| Concurrency | Different underlyings CAN fire concurrently in same 3s window |
| Auxiliary | `POST /v2/optionchain/expirylist` for expiry refresh |
| Response fields | `last_price`, `oc[strike]` map with `.ce` and `.pe` each containing `average_price`, `greeks.{delta,theta,gamma,vega}`, `implied_volatility`, `last_price`, `oi`, `previous_close_price`, `previous_oi`, `previous_volume`, `security_id`, `top_ask_price`, `top_ask_quantity`, `top_bid_price`, `top_bid_quantity`, `volume` |

**Operator-locked cadence: every 50 seconds.** Dhan allows up to ~1.2 req/sec per unique pair (1 per 3s), so 50s is conservative and rate-safe.

---

## §3. The Z+ 7-layer defense for the heart piece

| Layer | Mechanism | Catches | Latency |
|---|---|---|---|
| **L1 DETECT** | (a) Per-request `tv_option_chain_fetch_latency_ms{underlying}`; (b) per-request success/fail counter; (c) cache-age gauge `tv_option_chain_cache_age_seconds{underlying}` | Slow REST, failed REST, stale cache | per request ≤200ms |
| **L2 VERIFY** | (a) Compare option-chain `last_price` against live WS index LTP — must agree within 0.5%; (b) validate `oc` map has ≥ 30 strikes (sanity floor); (c) validate response status == "success" | Bad response, partial data, wrong expiry | per response ≤10ms |
| **L3 RECONCILE** | (a) Daily 15:31 IST: fetch full chain for all 3 underlyings, cross-verify against the day's intraday snapshots stored in `dhan_option_chain_raw`; (b) daily expirylist refresh at 15:31 IST | Silent corruption, missed Thursday rollover | daily |
| **L4 PREVENT** | (a) Pre-fetch token-age check (>4h remaining); (b) per-underlying rate-limit guard (refuse if last call < 3s ago); (c) pre-flight DNS resolution for api.dhan.co before market open | Wasted 401 calls, rate-limit violations, DNS failures mid-session | pre-cycle |
| **L5 AUDIT** | (a) Every request row → `option_chain_request_audit` table (ts, underlying, expiry, http_status, latency_ms, retry_count); (b) every response → `dhan_option_chain_raw` (full body) + `option_chain_snapshots` (parsed); (c) cache hit/miss tracked via Prom counter | Forensic replay, regulatory audit, anomaly hunting | async (off hot path) |
| **L6 RECOVER** | (a) DH-904 backoff ladder 10s→20s→40s→80s per `dhan-annexure-enums.md` rule 11; (b) network timeout → 1 retry with 2s delay; (c) concurrent fan-out means 1 failing underlying does NOT block the other 2; (d) **strategy fail-closed if cache age > 60s** (see §5) | All transient failures + protection from trading on stale data | within 60s |
| **L7 COOLDOWN** | (a) After 3 consecutive failed cycles, increase cadence to 60s → 90s → 120s; (b) reset to 50s on first success; (c) suppress duplicate Telegram alerts via 60s coalescer | Operator alert fatigue + Dhan API hammering | continuous |

---

## §4. The 8 failure modes (every one has a recovery)

| # | Failure | Detection | Recovery | Operator notified? |
|---|---|---|---|---|
| 1 | DH-904 rate limit | HTTP 429 / errorCode=DH-904 | Exponential backoff 10→20→40→80s; if 80s hits, skip cycle, emit High Telegram | Yes, on backoff entry |
| 2 | Network timeout (5s) | reqwest timeout | Retry once with 2s delay; on fail, mark underlying as STALE for this cycle | Only if 3 cycles stale |
| 3 | HTTP 5xx from Dhan | Status code | Same as #2 — retry once, then stale | Only if 3 cycles stale |
| 4 | Empty `oc` map | L2 verify | Reject response, retry once. If repeats → underlying expiry might have rolled mid-day → re-fetch expirylist | Yes |
| 5 | Cache stale > 60s during market hours | L1 gauge | **STRATEGY FAIL-CLOSED** — emit no signals until cache fresh; CRITICAL Telegram fires | Yes, Critical |
| 6 | Token expired mid-cycle (DH-901) | http_status 401 | Token-manager force-refresh; retry within same cycle | Yes if refresh fails |
| 7 | Cycle takes > 50s (overlap) | `tokio::Mutex<CycleState>` guard | Skip-next-cycle policy; next 50s tick is no-op; alarm fires if 2 skips in row | Yes on 2nd skip |
| 8 | Thursday 15:30 expiry rollover | L3 reconcile + intraday expirylist mismatch | Re-fetch expirylist at 15:31 IST; rebuild RAM cache for new nearest-expiry; pre-market next day uses new expiry | Info Telegram on rollover |

---

## §5. The strategy fail-closed gate (operator's "never trade on stale data")

```rust
// PSEUDOCODE — not for compilation
fn strategy_evaluate_per_minute() -> Option<TradingSignal> {
    let now_ist = ist_now();
    if !is_within_market_hours_ist(now_ist) {
        return None;
    }

    let max_cache_age_secs = 60;  // operator-locked
    for underlying in [NIFTY, BANKNIFTY, SENSEX] {
        let cache_age = option_chain_cache.age_seconds(underlying);
        if cache_age > max_cache_age_secs {
            // FAIL CLOSED
            emit_critical_telegram("Option chain stale {}s for {}", cache_age, underlying);
            metrics::counter!("tv_strategy_skipped_total", "reason" => "stale_option_chain").increment(1);
            return None;  // DO NOT TRADE
        }
    }

    // All 3 caches fresh — strategy can run
    ...
}
```

**Rationale:** trading on a stale option chain is worse than not trading. Better to miss an opportunity than to compute delta exposure with yesterday's IV.

**Constants pinned (would land in `crates/common/src/constants.rs`):**
```rust
pub const OPTION_CHAIN_FETCH_CADENCE_SECS: u64 = 50;
pub const OPTION_CHAIN_MAX_CACHE_AGE_SECS: u64 = 60;
pub const OPTION_CHAIN_REQUEST_TIMEOUT_SECS: u64 = 5;
pub const OPTION_CHAIN_RETRY_COUNT: u32 = 1;
pub const OPTION_CHAIN_BACKOFF_LADDER_SECS: [u64; 4] = [10, 20, 40, 80];
pub const OPTION_CHAIN_STALE_CYCLES_BEFORE_TELEGRAM: u32 = 3;
pub const OPTION_CHAIN_UNDERLYINGS: [(u32, &str); 3] = [
    (13, "NIFTY"),
    (25, "BANKNIFTY"),
    (51, "SENSEX"),
];
```

---

## §6. The audit + recapture chain (operator's "entire request and response")

Every cycle produces:

1. **Per-request audit row** (`option_chain_request_audit` QuestDB table):
   - ts, underlying_id, expiry, http_status, latency_ms, retry_count, error_class
2. **Full response blob** (`dhan_option_chain_raw` QuestDB table):
   - ts, underlying_id, expiry, json_body (TEXT), payload_hash (for dedup)
3. **Parsed snapshot** (`option_chain_snapshots` QuestDB table):
   - ts, underlying_id, expiry, strike, side (CE/PE), oi, ltp, iv, delta, theta, gamma, vega, volume, top_bid, top_ask
4. **Prometheus/CloudWatch metrics:**
   - `tv_option_chain_fetch_latency_ms{underlying}` histogram
   - `tv_option_chain_fetch_success_total{underlying}` counter
   - `tv_option_chain_fetch_failure_total{underlying, reason}` counter
   - `tv_option_chain_cache_age_seconds{underlying}` gauge
   - `tv_option_chain_cycle_skip_total{reason}` counter
   - `tv_strategy_skipped_total{reason}` counter
5. **CloudWatch Logs:**
   - INFO every cycle: "Option chain fetched: nifty=200ms 50strikes banknifty=180ms 48strikes sensex=220ms 45strikes"
   - WARN on retry
   - ERROR on cycle skip (with `code = ErrorCode::OptionChain05Stale.code_str()`)

**Replay path:** at any time, the operator (or post-mortem investigator) can SELECT from `dhan_option_chain_raw` for any ts range and replay the strategy's view of the world.

DEDUP keys (Z+ uniqueness):
- `option_chain_request_audit`: `(ts, underlying_id, expiry)`
- `dhan_option_chain_raw`: `(ts, underlying_id, expiry, payload_hash)`
- `option_chain_snapshots`: `(ts, underlying_id, expiry, strike, side)`

S3 lifecycle: 14-day hot in QuestDB → 90-day cold in S3 → 5y SEBI archive in Glacier Deep.

---

## §7. The 6 new alarms (CloudWatch Alarms → SNS 3-leg)

| # | Alarm | Trigger | Severity |
|---|---|---|---|
| 1 | `tv-option-chain-cache-stale-60s` | `tv_option_chain_cache_age_seconds > 60` for any underlying during market hours | Critical |
| 2 | `tv-option-chain-cycle-skips-3-in-5m` | `increase(tv_option_chain_cycle_skip_total[5m]) > 3` | High |
| 3 | `tv-option-chain-dh904-sustained` | DH-904 backoff laddering ≥ 80s | High |
| 4 | `tv-option-chain-l2-verify-fail` | LTP mismatch vs WS or empty `oc` map | Medium |
| 5 | `tv-strategy-skipped-due-stale-chain` | `increase(tv_strategy_skipped_total[5m]) > 5` | Critical |
| 6 | `tv-option-chain-rollover-detected` | Mid-day expirylist change | Info |

All edge-triggered (operator-charter Rule 4). All wrapped in `increase()` / `rate()` (audit-findings Rule 12). All gated by `tv_market_hours_active == 1`.

---

## §8. New ErrorCode variants needed (per operator-charter rule 5 + 11)

Each gets a runbook entry under `.claude/rules/project/option-chain-error-codes.md`.

| ErrorCode | Severity | Auto-triage safe |
|---|---|---|
| `OPTION-CHAIN-01-FETCH-FAILED` | High | Yes (transient) |
| `OPTION-CHAIN-02-DH904-EXHAUSTED` | High | No (sustained rate limit) |
| `OPTION-CHAIN-03-PARSE-FAILED` | Medium | Yes |
| `OPTION-CHAIN-04-VERIFY-FAILED-VS-WS` | Medium | No (data integrity) |
| `OPTION-CHAIN-05-CACHE-STALE-HALT-STRATEGY` | Critical | No (strategy halted) |
| `OPTION-CHAIN-06-CYCLE-OVERLAP-SKIP` | High | Yes (transient) |
| `OPTION-CHAIN-07-EXPIRY-ROLLOVER` | Info | Yes |
| `OPTION-CHAIN-08-TOKEN-EXPIRED-MID-CYCLE` | Critical | No (auth issue) |

---

## §9. The 4-channel deadman notification for this subsystem

Per operator's "logging + SMS + call + Telegram" demand:

```
Cache age > 60s during market hours
            │
            ▼
   ErrorCode = OPTION-CHAIN-05
            │
   ┌────────┴────────┐
   ▼                 ▼
ERROR log    Strategy fail-closed
in CW Logs   (skip next minute decision)
            │
            ▼
   CW Alarm `tv-option-chain-cache-stale-60s` fires
            │
            ▼
   SNS topic tv-prod-alerts
            │
   ┌────────┼────────┐
   ▼        ▼        ▼
  SMS    Telegram   Email
 (DLT)    (Lambda)
 ~₹0.24   free      free
```

---

## §10. Common-runtime / dynamic / scalable / automated (operator-charter §A)

| Demand | How the option chain loop satisfies |
|---|---|
| Common runtime | Same Rust code Mac dev = AWS prod; same `[option_chain]` TOML config |
| Dynamic | Cadence + underlyings list both config-driven; backoff ladder constant-pinned |
| Scalable | If operator adds a 4th underlying later, same code handles it (concurrent fan-out, just adds a 4th JoinHandle) |
| Automated | Spawned at boot, no manual trigger; respects market hours; auto-recovers from DH-904 |
| Tracked | 5 metrics + 3 tables + CW Logs + 6 alarms |
| Audited | DEDUP UPSERT KEYS on all 3 tables; S3 lifecycle → 5y SEBI |
| Real-time | Cache age visible in CloudWatch dashboard at 1s resolution (paid metric tier if needed) |
| Notified on Telegram | 6 alarms route via SNS Lambda → Telegram bot |

---

## §11. Honest envelope claim (operator-charter §F)

> "Inside the tested envelope: option chain is fetched every 50s for 3 underlyings concurrently; cache freshness is monitored continuously; strategy fail-closes within 60s of staleness; every request and response is audited to QuestDB and replayable from `dhan_option_chain_raw`. Beyond the envelope (Dhan API regional outage > 60s, simultaneous network failure of both AWS and Dhan endpoints, JWT compromise mid-session), the strategy refuses to emit signals AND the operator is paged within 60 seconds via 3 independent SNS legs (SMS+Telegram+Email).
>
> **NOT promised:** literal "never miss a fetch" — Dhan's REST API has no published SLA, network blips happen. What IS promised: detection within 30s, alert within 60s, strategy fail-closed within 90s, full audit trail for replay."

---

## §12. The 9-box per-item checklist (per operator-charter §C)

For when this design becomes an implementation plan:

| Box | Status |
|---|---|
| 1. Typed event (NotificationEvent variant) | NEW: `OptionChainStaleHaltStrategy`, `OptionChainBackoffExhausted`, ... |
| 2. ErrorCode | 8 new variants in §8 |
| 3. Tracing macro with `code` field | required at all 8 emission points |
| 4. Prometheus counter | 5 metrics in §6 |
| 5. CloudWatch alarm | 6 alarms in §7 |
| 6. SNS routing | 3-leg fan-out per §9 |
| 7. Call site exists | Spawned at boot step (new Step 9b in `main.rs`) |
| 8. Triage YAML rule | `.claude/triage/option-chain-rules.yaml` (new) |
| 9. Ratchet test | `crates/core/tests/option_chain_z_plus_guard.rs` pins constants + emission points |

---

## §13. Open questions for operator

1. **Strategy fail-closed threshold** — locked at 60s above. Operator OK or want different (e.g., 30s / 120s)?
2. **Cadence** — 50s locked. Operator OK or want faster (e.g., every 30s) since Dhan allows ~3s per unique pair?
3. **Verify-against-WS tolerance** — 0.5% LTP agreement between option chain `last_price` and live WS index LTP. Operator OK or want tighter/looser?
4. **Expirylist refresh time** — daily at 15:31 IST (post-close). Operator OK?

---

## §14. Trigger / auto-load paths

This rule activates when editing:
- `crates/core/src/option_chain/*.rs` (will be created)
- `crates/trading/src/strategy/*.rs` (fail-closed gate)
- `crates/common/src/constants.rs` (cache age constants)
- Any file containing `OptionChainFetcher`, `option_chain_cache`, `OPTION_CHAIN_MAX_CACHE_AGE_SECS`
