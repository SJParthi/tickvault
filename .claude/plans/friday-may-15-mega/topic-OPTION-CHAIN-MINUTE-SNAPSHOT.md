# Topic — Option-Chain Minute-Snapshot Pipeline

> **Status:** APPROVED 2026-05-16. Operator-locked across 3 sessions.
> **Authority:** CLAUDE.md > operator-charter-forever.md §C 15-row matrix > this file.
> **Owner:** Parthiban (architect). Claude Code (builder).
> **Reference:** Dhan Option Chain API doc shared verbatim in chat 2026-05-16.

## 1. What this pipeline does

Three times per minute during NSE market hours `[09:15, 15:30) IST`, the
system fetches the option chain (current expiry) for the 3 underlyings —
SENSEX, BANKNIFTY, NIFTY — at staggered slots that respect Dhan's
"1 unique request per 3 seconds" rate-limit rule.

The freshest snapshot lives in a RAM cache (papaya HashMap). The
strategy reads from the cache; it NEVER awaits the fetch. Persisted
to QuestDB for SEBI 5y forensic chain. Operator gets edge-triggered
Telegram on failure; hard-halts the strategy on 5 minutes of
sustained cache staleness.

## 2. The locked schedule

| IST tick | Underlying | UnderlyingScrip | Segment | Slot rationale |
|---|---|---|---|---|
| `HH:MM:53` | SENSEX | 51 | `IDX_I` | BSE first, isolated |
| `HH:MM:56` | BANKNIFTY | 25 | `IDX_I` | 3s gap respects Dhan rule |
| `HH:MM:59` | NIFTY | 13 | `IDX_I` | Final slot — closest to minute boundary |

3s per-underlying gap is operator's preferred defensive spacing over
the alternative "fire 3 concurrent at :57" (which Dhan also allows but
is less debuggable + has TCP/TLS connection burst risk).

## 3. The 5-Layer Defense Stack

| Layer | Mechanism | Triggers on |
|---|---|---|
| **L1 Primary fetch** | Scheduler fires at slot_sec | Every minute, every underlying |
| **L2 Same-minute retry** | Up to 2 retries per underlying, ≥ 3s apart | DH-904 / 5xx / timeout from L1 |
| **L3 RAM cache fallback** | papaya HashMap, last-known-good snapshot | L1 + L2 both exhausted |
| **L4 Strategy reads cache** | Strategy NEVER awaits fetch | Always — even on happy path |
| **L5 Hard staleness halt** | Strategy refuses new entries; Severity::Critical Telegram | Cache age > `cache_max_stale_secs` (default 300) |

## 4. Config-driven from day one (NON-NEGOTIABLE)

Hardcoding underlyings or slot times violates operator-charter "dynamic +
scalable" principles. The TOML config below is the SOLE source of truth
for which underlyings + when:

```toml
[option_chain_minute_snapshot]
enabled = true
cache_max_stale_secs = 300        # 5 min hard halt threshold
strike_window_atm_each_side = 25  # ATM ± 25 strikes kept in RAM cache
fetch_retry_max_attempts = 2      # same-minute retry budget per underlying

[[option_chain_minute_snapshot.underlyings]]
symbol      = "SENSEX"
security_id = 51
segment     = "IDX_I"
slot_sec    = 53

[[option_chain_minute_snapshot.underlyings]]
symbol      = "BANKNIFTY"
security_id = 25
segment     = "IDX_I"
slot_sec    = 56

[[option_chain_minute_snapshot.underlyings]]
symbol      = "NIFTY"
security_id = 13
segment     = "IDX_I"
slot_sec    = 59
```

Adding MIDCPNIFTY tomorrow = edit TOML, restart app. Zero code change.

## 5. QuestDB schema

```sql
CREATE TABLE IF NOT EXISTS option_chain_minute_snapshot (
    ts                      TIMESTAMP,
    trading_date_ist        TIMESTAMP,
    underlying_symbol       SYMBOL,
    underlying_security_id  INT,
    exchange_segment        SYMBOL,
    expiry                  DATE,
    strike                  DOUBLE,
    ce_or_pe                SYMBOL,
    security_id             INT,
    last_price              DOUBLE,
    average_price           DOUBLE,
    oi                      LONG,
    previous_oi             LONG,
    volume                  LONG,
    previous_volume         LONG,
    previous_close_price    DOUBLE,
    top_bid_price           DOUBLE,
    top_bid_quantity        LONG,
    top_ask_price           DOUBLE,
    top_ask_quantity        LONG,
    implied_volatility      DOUBLE,
    greek_delta             DOUBLE,
    greek_theta             DOUBLE,
    greek_gamma             DOUBLE,
    greek_vega              DOUBLE,
    cache_age_used_secs     INT,
    fetch_outcome           SYMBOL
) TIMESTAMP(ts) PARTITION BY DAY WAL
DEDUP UPSERT KEYS(
    trading_date_ist, ts, underlying_symbol, exchange_segment,
    strike, ce_or_pe
);
```

DEDUP key includes `(strike, ce_or_pe)` so 50+ strikes × 2 sides don't
overwrite each other. Includes `exchange_segment` per I-P1-11
composite-key uniqueness. Includes `trading_date_ist` per the
2026-04-28 regression class fix.

SEBI 90d hot → S3 IT → Glacier per `aws-budget.md`.

## 6. PR rollout (5 PRs, each ≤ 30 files, serial-PR protocol)

| PR # | Scope | Files |
|---|---|---|
| **#1 Foundation (this PR)** | Plan + config schema + validator + 12 ratchet tests | `~150 LoC` |
| #2 Persistence | `option_chain_minute_snapshot_persistence.rs` + DDL boot wire + 6 schema ratchets | ~300 LoC |
| #3 Dhan REST client | `option_chain/dhan_client.rs` + expirylist + optionchain + 4 fixture tests | ~250 LoC |
| #4 Cache + scheduler | papaya cache + per-slot tokio task + 4 Telegram variants + 4 counters | ~400 LoC |
| #5 Strategy integration + verification | Strategy reads cache, 5-min halt, `make verify-option-chain-minute-snapshot` | ~250 LoC |

## 7. Telegram variants (4 new, defined in PR #4)

| Variant | Severity | Fires on |
|---|---|---|
| `OptionChainFetchFailed` | High | First failure of a slot per minute (edge-triggered) |
| `OptionChainCacheFallback` | Medium | Strategy reads stale cache (informational) |
| `OptionChainStaleHalt` | Critical | Cache age > `cache_max_stale_secs` |
| `OptionChainConfigInvalid` | Critical | Boot-time validator rejects config |

## 8. Prometheus counters (4 new, defined in PR #4)

| Counter | Labels | Purpose |
|---|---|---|
| `tv_option_chain_fetches_total` | `underlying`, `outcome` (fresh / retry / cache_fallback) | Sanity vs row count |
| `tv_option_chain_fetch_errors_total` | `underlying`, `error_class` | Failure mode classification |
| `tv_option_chain_cache_age_secs` | `underlying` (gauge) | Real-time freshness |
| `tv_option_chain_strategy_halts_total` | (none) | 5-min halt occurrences |

## 9. Verification protocol (PR #5)

`make verify-option-chain-minute-snapshot` runs 7 SQL queries via the
`tickvault-logs` MCP tools:

1. `SHOW COLUMNS` → 27 columns present + types match
2. `count_distinct(strike) per underlying` → ~50-80 strikes each
3. `min/avg/max(last_price)` → spot within realistic band per underlying
4. `null counts` → no NULL IV/delta/oi for liquid strikes
5. `DEDUP duplicates` → zero rows in the GROUP BY HAVING > 1 query
6. `fetch_outcome distribution` → `fresh ~99%`, others < 1%
7. Prometheus counters cross-check vs QuestDB row counts

## 10. Z+ 15-row "100% Everything" Matrix

| Dimension | Mechanical proof artefact |
|---|---|
| Code coverage | Per-PR unit tests, `quality/crate-coverage-thresholds.toml` 100% min |
| Audit coverage | `option_chain_minute_snapshot` table with DEDUP UPSERT KEYS |
| Testing coverage | Unit + fixture + integration + chaos (PR #5) |
| Code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify gates |
| Code performance | papaya O(1) cache read; ILP cold-path persist; Criterion bench in PR #4 |
| Monitoring | 4 Prometheus counters + 1 gauge per dimension |
| Logging | `error!` with `code = ErrorCode::*.code_str()` per audit-findings Rule 5 |
| Alerting | 4 Telegram variants + 1 Prometheus alert rule per failure mode |
| Security | SSM-only Dhan token; `Secret<String>` zeroize on drop |
| Security hardening | Token never in logs/labels; ILP injection prevented via sanitize_audit_string |
| Bug fixing | Adversarial 3-agent review on PR #4 (the hot-path one) |
| Scenarios covering | 5-layer defense covers DH-904, 5xx, timeout, stale cache, config error |
| Functionalities covering | Every pub fn has test or `// TEST-EXEMPT:` |
| Code review | 3-agent review per PR; operator approval gate |
| Extreme check | Boot-time config validator HALTS app on invalid TOML |

## 11. Z+ 7-row Resilience Demand Matrix

| Demand | Honest envelope |
|---|---|
| Zero ticks lost | N/A — this pipeline reads option chain, not ticks |
| WS never disconnects | N/A — REST-only pipeline; WS spot feed is independent |
| Never slow/locked/hanged | Strategy reads cache, never awaits fetch. Cache read is O(1) papaya |
| QuestDB never fails | ILP persist is cold path; failure logged but does not block cache or strategy |
| O(1) latency | papaya cache `get` is O(1) |
| Uniqueness + dedup | DEDUP key includes 5 composite columns + segment |
| Real-time proof | 7-layer telemetry per PR |

## 12. Honest 100% claim

"100% inside the tested envelope, with ratcheted regression coverage:
the 3-slot schedule respects Dhan's 1 unique request per 3 seconds
rate limit; the 5-layer defense covers every realistic failure mode
short of Dhan API down for ≥ 5 minutes; the boot-time validator
HALTS on invalid config. Beyond the envelope, this pipeline does
NOT guarantee the Dhan API itself returns within latency targets —
when it doesn't, the strategy reads from the RAM cache. If Dhan is
down for ≥ 5 min, the strategy refuses new entries (Severity::Critical
Telegram). Existing positions are held until operator decision
per `docs/runbooks/kill-switch.md`."
