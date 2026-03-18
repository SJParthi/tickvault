# Instrument Gaps — Consolidated Reference

> **Purpose:** Single source of truth for ALL instrument-related gaps across
> V1 (Comprehensive), V2 (Deep Scan), V3 (Cross-Reference), and live
> architecture review sessions. Deduplicated and severity-ranked.
>
> **Last updated:** 2026-03-11 (rev 2)
> **Sources:** Gap analysis sessions V1-V3 (archived in git history), live architecture review sessions (2)

---

## P0 — CRITICAL (Will cause wrong trades or data corruption)

| # | ID | Gap | Impact | Source |
|---|---|---|---|---|
| 1 | **I-P0-01** | **Duplicate security_id in CSV = warn+skip, not hard error.** HashMap silently overwrites. Could keep wrong contract and trade it. | Wrong contract traded | V3: P0-GAP-I-01 |
| 2 | **I-P0-02** | **No count consistency check after build.** If CSV is truncated (50 rows vs 50,000), system continues. Validation check 11 in requirements but NOT in code. | Trading with incomplete universe | V3: P0-GAP-I-02 |
| 3 | **I-P0-03** | **No expiry_date >= today check at order placement (Gate 4).** Universe filters expired contracts, but no final guard at OMS before sending to Dhan. Stale universe = expired contract order. | Order rejected by exchange or worse | Architecture review |
| 4 | **I-P0-04** | **No persistent storage for instrument cache.** Cache dir `/app/data/instrument-cache` is on ephemeral host filesystem. Instance termination = cache GONE. During market hours, no cache = no instruments = no trading. On Mac: local SSD (survives). On AWS: must be EBS-backed volume. | Total trading day loss on instance replace | Storage survival review |
| 5 | **I-P0-05** | **No S3 remote backup of rkyv/CSV cache.** If EBS volume is lost (corruption, AZ failure), no remote recovery path. S3 backup enables cross-AZ, cross-instance recovery. Without it, a fresh instance during market hours = dead. | No disaster recovery for instruments | Storage survival review |
| 6 | **I-P0-06** | **No emergency download override when all caches missing during market hours.** Code returns `Unavailable` with INFO log when rkyv + CSV both missing. Should force-download as last resort (3-10s latency beats zero instruments). | System runs with zero instruments silently | Storage survival review |

---

## P1 — HIGH (Will cause degraded operation or silent failures)

| # | ID | Gap | Impact | Source |
|---|---|---|---|---|
| 4 | **I-P1-01** | **No automated daily CSV refresh.** System relies on daily restart. Running across days = stale instruments. New contracts missing, expired contracts still present. | Trading stale instruments | V1: GAP-CFG-01, V3: P1-GAP-I-03 |
| 5 | **I-P1-02** | **Delta detector only tracks 2 fields** (lot_size, expiry). Misses: strike_price, option_type, tick_size, segment, display_name changes. | Silent field changes go undetected | V3: P1-GAP-I-04 |
| 6 | **I-P1-03** | **Security_id reuse across different underlyings not detected.** If Dhan reuses security_id 2885 (was RELIANCE, now NEWSTOCK), delta detector sees separate add/expire. No compound identity match (symbol + expiry + strike + type). | Incorrect historical attribution | V3: P1-GAP-I-05, Architecture review |
| 7 | **I-P1-04** | **Security_id reassignment for SAME contract not detected.** If NIFTY 24000 CE changes from security_id 48372 to 55000, system sees two unrelated events. Should match by contract identity and emit `security_id_reassigned` event. | Lost position tracking, broken subscriptions | Architecture review |
| 8 | **I-P1-05** | **Historical queries break on security_id reuse.** If security_id 2885 was RELIANCE in March and NEWSTOCK in June, `WHERE security_id=2885` returns mixed data. Need compound key (security_id + symbol) or date-bounded queries. | Corrupted backtesting/analysis | Architecture review |
| 9 | **I-P1-06** | **Tick DEDUP key may collide across segments.** QuestDB DEDUP uses `(ts, security_id)`. If same security_id exists in NSE_EQ and BSE_EQ (implicit Dhan guarantee, not explicit), ticks merge/overwrite. | Silent tick data loss | V3: QDB-GAP-02 |
| 10 | **I-P1-07** | **Unavailable = INFO log instead of CRITICAL halt + Telegram alert.** When no instruments are available, `main.rs:1011` logs INFO and continues with `(None, None)`. System runs with zero instruments — should be hard HALT with Telegram CRITICAL alert. | System silently runs without instruments | Storage survival review |
| 11 | **I-P1-08** | **~~Cross-day snapshot accumulation in QuestDB.~~ RESOLVED.** Snapshot tables (`fno_underlyings`, `derivative_contracts`, `subscribed_indices`) accumulate rows across days by design (DEDUP only deduplicates within same timestamp). Unfiltered Grafana queries show doubled/tripled counts. Fixed: (1) Historical snapshots PRESERVED (no DELETE — audit trail + security_id reuse tracking), (2) All Grafana queries filter by `WHERE timestamp = (SELECT max(timestamp) FROM table)`, (3) `verify_instrument_row_counts()` filters by today's snapshot. 13 regression tests + mechanical enforcement hook prevent recurrence. | Doubled Grafana counts, misleading dashboards | Post-mortem 2026-03-15 |

---

## P2 — MEDIUM (Should fix before production)

| # | ID | Gap | Impact | Source |
|---|---|---|---|---|
| 10 | **I-P2-01** | **No `index_atm_strikes_above/below` config.** Config has `stock_atm_strikes_above = 10` but no equivalent for indices. NIFTY/BANKNIFTY need wider strike ranges than individual stocks. | Suboptimal index option coverage | V3: SUBPLAN-GAP-01 |
| 11 | **I-P2-02** | **No `is_trading_day()` guard on instrument download.** CSV downloads on weekends/holidays too. By design for crash recovery, but wastes bandwidth. | Unnecessary network traffic | V3: P1-GAP-I-06, CROSS-GAP-06 |
| 12 | **I-P2-03** | **No trading day guard on order placement.** If system runs over weekend, no check prevents strategy evaluation on non-trading day. Practically safe since Dhan won't send ticks on weekends. | Theoretical false signals | V2: NEW-01 |
| 13 | **I-P2-04** | **No `is_trading_day()` check before EOD squareoff.** Squareoff triggers purely on time (15:29 IST). On non-trading day, would attempt to squareoff non-existent positions. | Harmless but unnecessary API calls | V2: NEW-03 |
| 14 | **I-P2-05** | **No `sequence_number` in tick dedup.** Requirements doc says dedup by `(security_id, exchange_timestamp, sequence_number)`. Code dedup uses `(ts, security_id)` only. Binary header has unknown u16 that might be sequence number but is never parsed. | Sub-second tick dedup may drop valid ticks | V3: WS-GAP-04 |

---

## P3 — LOW (Nice to have)

| # | ID | Gap | Impact | Source |
|---|---|---|---|---|
| 15 | **I-P3-01** | **`segment_code_to_str()` duplicated in 3 files.** Same function copy-pasted. New segment code requires updating all 3. | Maintenance burden | V3: QDB-GAP-06 |

---

## Summary

| Severity | Count | Key Risk |
|---|---|---|
| **P0-CRITICAL** | 6 | Wrong trades, data corruption, expired contract orders, cache survival |
| **P1-HIGH** | 8 (1 resolved) | Stale instruments, silent ID reuse, mixed historical data, silent failures, cross-day snapshot accumulation |
| **P2-MEDIUM** | 5 | Missing config, weekend guards, dedup gaps |
| **P3-LOW** | 1 | Code duplication |
| **TOTAL** | **20 (1 resolved)** | |

---

## Fix Priority

### Before Any Live Trade (P0)

1. **I-P0-01** — Duplicate security_id = hard error (reject build)
2. **I-P0-02** — Count consistency check after build
3. **I-P0-03** — Expiry check at order placement (Gate 4)
4. **I-P0-04** — Persistent storage (EBS on AWS, local SSD on Mac)
5. **I-P0-05** — S3 remote backup of rkyv/CSV cache
6. **I-P0-06** — Emergency download override when all caches missing

### Before Production (P1)

7. **I-P1-01** — Daily CSV refresh scheduler (08:45 IST timer)
8. **I-P1-07** — Unavailable = CRITICAL halt + Telegram alert (not INFO)
9. **I-P1-03** — Security_id reuse detection (compound identity match)
10. **I-P1-04** — Security_id reassignment detection
11. **I-P1-02** — Delta detector full field coverage (all 6+ fields)
12. **I-P1-05** — Compound keys in historical queries
13. **I-P1-06** — Add segment to tick DEDUP key

### Before Scale (P2)

14. **I-P2-01** — Index ATM strike config
15. **I-P2-02** — Trading day guard on download
16. **I-P2-03** — Trading day guard on order placement
17. **I-P2-04** — Trading day guard on EOD squareoff
18. **I-P2-05** — Sequence number in tick dedup

### Maintenance (P3)

19. **I-P3-01** — Deduplicate segment_code_to_str()

---

## RESOLVED (Previously identified, now fixed)

| ID | Gap | Resolution |
|---|---|---|
| ~~GAP-WS-01~~ | No dynamic subscribe/unsubscribe | **RESOLVED** — mpsc channel in read loop |
| ~~GAP-WS-05~~ | Disconnect codes incomplete | **RESOLVED** — all 12 codes (801-814) mapped |
| ~~I-P1-08~~ | Cross-day snapshot accumulation in QuestDB | **RESOLVED** (2026-03-15, rev 2) — 3-prong fix: (1) Historical snapshots PRESERVED across days (no DELETE — enables audit trail, security_id reuse tracking, SEBI compliance), (2) all 12 Grafana instrument queries filter by `WHERE timestamp = (SELECT max(timestamp) FROM table)`, (3) `verify_instrument_row_counts()` filters by today's snapshot. 13 regression tests + mechanical enforcement hook (blocks `DELETE FROM` on snapshot tables) prevent recurrence. Data retention via QuestDB partition management (DETACH PARTITION), not application DELETE. |
