# GDF 1-Second SubscribeRealtime Feed — Design (99-symbol universe)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` >
> `.claude/rules/project/gdf-third-feed-scope-2026-07-13.md` (the governing lock) >
> `.claude/plans/active-plan-gdf-feed.md` (the approved plan + the 2026-07-21 operator
> amendment) > this document.
> **What this is:** the DESIGN for the GDF **SubscribeRealtime (mode B)** live capture
> per the operator's 2026-07-21 order. The operator's verbatim quotes live in the plan
> amendment (branch `claude/gdf-1s-subscribe-plan`) — referenced here, never duplicated.
> **Status:** DESIGN — code lands via the plan's PR series (PR-B..PR-G), each behind the
> design-first wall. `feeds.gdf_enabled = false` DEFAULT stands. No strategy wiring, no
> order wiring (§28 boundary + dry_run untouched). Native tickvault Rust only — NOTHING
> vendored from the GFDL Python/JS SDKs (reference-only, the Groww §32/§35 precedent).
> **Standing bans carried verbatim:** no GDF REST callers ever
> (`no-rest-except-live-feed-2026-06-27.md` posture + scope-lock §3); GDF Greeks
> endpoints stay FORBIDDEN (scope-lock §3 — BruteX computes greeks from our own capture).
> **Evidence tiers:** **[R#n]** = row n of the reconciled claims table in
> `docs/gdf-ref/README.md`; **[U-n]** = `docs/gdf-ref/99-UNKNOWNS.md`; LIVE-DOC / SDK /
> ARITH / Assumed / Unknown per the README legend. Unverified facts are labeled, never
> asserted (zero-loss charter §4).

---

## §1. Scope & universe — exactly 99 subscriptions

| Aspect | Locked value |
|---|---|
| Mode | **SubscribeRealtime (mode B)** — one subscribe request per symbol, no batch form [R#12]; the firehose (mode A, StreamAllSymbols) is specced in the plan but DEFERRED for this design |
| Universe | **Exactly 99 symbols:** 1 × NIFTY 50 spot index (NSE_IDX `NIFTY 50`) + the current-expiry NIFTY option chain — **49 CE + 49 PE strikes** centred on the reference spot (§8) |
| Expansion | NEVER the whole exchange; NEVER a 2nd underlying; NEVER a 2nd expiry — each requires a fresh dated operator quote in the scope lock FIRST |
| Greeks | **NOT from GDF.** BruteX computes greeks from the per-second spot + option LTP we capture. The GDF Greeks family (GetGreeks etc.) stays FORBIDDEN per scope-lock §3 — no code path may call it |
| Cadence (vendor ceiling) | ~1 conflated full-image L1 update per second per symbol [R#12] — the vendor's ceiling, never hidden (§13) |
| Steady message rate | ≈ 99 msg/s (ARITH: 99 symbols × ~1/s); burst envelope in §4 |

## §2. Wire contract — FULL FIELD CAPTURE (operator mandate 1)

Every field GDF sends is captured and persisted. The `RealtimeResult` long-key push row
(LIVE-DOC, `docs/gdf-ref/03-websocket-realtime-feed.md`) carries ~23 fields; the decoder
treats EVERY tick-level field as `Option`-al (2018-era rows omit PriceChange — [R#16]),
tolerates scientific-notation floats [R#17] and non-JSON plain-text diagnostic frames
[R#8], and NEVER panics on an unknown discriminant.

### §2.1 Field inventory + column binding (every field has a destination)

| # | GDF field | Meaning (LIVE-DOC) | Destination |
|---|---|---|---|
| 1 | `Exchange` | segment token (`NSE`/`NFO`/`NSE_IDX`…) | mapped → `exchange_segment` (both tables) via the §7 segment map |
| 2 | `InstrumentIdentifier` | native symbol string | `instrument_lifecycle.symbol_name` (feed='gdf') + sidecar `identifier` |
| 3 | `LastTradeTime` (LTT) | trade time, **UTC epoch SECONDS** [R#15] | `ticks.ts` (µs, second-floored) + sidecar `ltt` |
| 4 | `ServerTime` (STT) | server stamp, epoch seconds | sidecar `stt`; `STT − LTT` feeds the lag gauge (LAG_FLOOR_MS_GDF = 1000) |
| 5 | `LastTradePrice` | LTP | `ticks.ltp` + sidecar `ltp` |
| 6 | `LastTradeQty` | last trade qty (0 on quote-refresh seconds — LIVE-DOC) | sidecar `ltq` |
| 7 | `AverageTradedPrice` | day ATP/VWAP | sidecar `atp` |
| 8 | `BuyPrice` / `BuyQty` | L1 best bid + qty | sidecar `bid`, `bid_qty` |
| 9 | `SellPrice` / `SellQty` | L1 best ask + qty | sidecar `ask`, `ask_qty` |
| 10 | `Open` / `High` / `Low` | DAY-cumulative OHL | sidecar `day_open`, `day_high`, `day_low` |
| 11 | `Close` | **PREVIOUS day's close** in the realtime family [R#14] — never a bar close | sidecar `prev_close` |
| 12 | `OpenInterest` | OI (0 for cash/index; inline every second) | `ticks.oi` + sidecar `oi` |
| 13 | `OpenInterestChange` | OI delta (Option-al) | sidecar `oi_change` |
| 14 | `QuotationLot` | lot size (CAN change — never hardcode) | sidecar `lot`; drift vs the morning master = coded anomaly |
| 15 | `TotalQtyTraded` | cumulative day volume | sidecar `total_qty`; volume DELTAS feed §5 |
| 16 | `Value` | day turnover | sidecar `value` |
| 17 | `PreOpen` | pre-open flag (bool; string-boolean drift tolerated [R#27]) | sidecar `pre_open` |
| 18 | `PriceChange` / `PriceChangePercentage` | vs prev close (Option-al, absent 2018-era) | sidecar `price_chg`, `price_chg_pct` |
| 19 | `MessageType` | envelope discriminant | decode routing only (not persisted) |

Binding classes: (a) the **core tick row** → the SHARED `ticks` table, `feed='gdf'`,
DEDUP `(ts, security_id, segment, capture_seq, feed)` (feed-in-key EVERYWHERE per
`data-integrity.md`; I-P1-11 composite identity §7); (b) the **full-fidelity L1 row** →
the PROPOSED sidecar below. NO field is dropped.

### §2.2 PROPOSED sidecar `gdf_l1_snapshots` — HONESTY FLAG (scope-lock carve-out)

**HONEST:** `gdf-third-feed-scope-2026-07-13.md` §5 REJECTS "any `gdf_*` parallel DATA
table". This sidecar is therefore presented as a **PROPOSED carve-out** that may land
ONLY after a dated operator edit to that rule file FIRST (the rule-file-first law). It is
NOT a parallel copy of `ticks` — it is the full-fidelity L1 superset row no shared table
carries today.

DDL sketch (QuestDB; self-healing `ALTER ADD COLUMN IF NOT EXISTS` on boot):

```sql
CREATE TABLE IF NOT EXISTS gdf_l1_snapshots (
  ts TIMESTAMP, security_id LONG, exchange_segment SYMBOL, feed SYMBOL,
  identifier SYMBOL, ltt TIMESTAMP, stt TIMESTAMP,
  ltp DOUBLE, ltq LONG, atp DOUBLE,
  bid DOUBLE, bid_qty LONG, ask DOUBLE, ask_qty LONG,
  day_open DOUBLE, day_high DOUBLE, day_low DOUBLE, prev_close DOUBLE,
  oi LONG, oi_change LONG, lot INT, total_qty LONG, value DOUBLE,
  pre_open BOOLEAN, price_chg DOUBLE, price_chg_pct DOUBLE,
  capture_seq LONG
) TIMESTAMP(ts) PARTITION BY DAY WAL
  DEDUP UPSERT KEYS (ts, security_id, exchange_segment, feed, capture_seq);
```

**Alternative considered (kept honest):** nullable extra columns on the shared `ticks`
table. Cost: ~18 wide always-null columns for every Dhan/Groww row, schema churn on the
one shared hot table every feed writes, and a fatter hot-path ILP line for all feeds —
rejected on the merits, but the sidecar still needs the rule-file edit first.

**Test rows (one per binding class):** T-W1 core-row round trip — one RealtimeResult
frame → exactly one `ticks` row with feed='gdf' + DEDUP idempotent on replay; T-W2
sidecar round trip — the same frame → one `gdf_l1_snapshots` row with all 25 columns
populated / honest NULL where the vendor omitted; T-W3 2018-era row (missing
PriceChange) decodes + persists with NULLs, never a decode error.

## §3. Architecture (operator mandate 3)

```
            wss?://<endpoint>  (plain ws:// on the trial — §12 flag)
                    │
                    ▼
   ┌────────────────────────────────┐
   │  GDF WS read loop (1 conn)     │  tokio-tungstenite 0.29 (pinned),
   │  frame cap LIFTED [R#26]       │  no blocking I/O in the loop
   └────────────┬───────────────────┘
                │ raw frame bytes
                ▼
   ┌────────────────────────────────┐
   │ 1. WAL append (capture-at-     │  data/spill/gdf/ — BEFORE any parse
   │    receipt, BEFORE parse)      │  O(1)/msg, sequential append
   └────────────┬───────────────────┘
                ▼
   ┌────────────────────────────────┐
   │ 2. Tolerant decode             │  Option-al fields, sci-notation,
   │    (unknown/non-JSON → DLQ)    │  never panic; zero alloc steady-state
   └────────────┬───────────────────┘
                ▼
   ┌────────────────────────────────┐
   │ 3. Identity + segment map (§7) │  O(1) lookup, papaya map
   └────────────┬───────────────────┘
        ┌───────┴────────┐
        ▼                ▼
 ┌─────────────┐  ┌──────────────────┐
 │ 4a. ticks   │  │ 4b. gdf_l1_      │   bounded ring → NDJSON spill →
 │  (shared,   │  │  snapshots       │   DLQ between stages 3 and 5
 │  feed=gdf)  │  │  (PROPOSED §2.2) │   (the proven absorb chain, §4)
 └──────┬──────┘  └────────┬─────────┘
        └───────┬──────────┘
                ▼
   ┌────────────────────────────────┐
   │ 5. In-process broadcast        │  bounded channel, no unbounded queues
   └────────────┬───────────────────┘
                ▼
   ┌────────────────────────────────┐
   │ 6. Per-frame RAM aggregators   │  20 frames × 99 instruments (§5, §9)
   │    (RAM-first — LAW)           │  O(1)/msg per frame, zero alloc
   └────────────┬───────────────────┘
                ▼
   ┌────────────────────────────────┐
   │ 7. Async QuestDB ILP writer    │  cold path; ticks + sidecar +
   │    + metrics / alerts / audit  │  candles_1m only (§9.3)
   └────────────────────────────────┘
```

Per-stage law: O(1) per message, zero hot-path allocation (the three principles), no
blocking I/O anywhere in the read loop; QuestDB writes are async and OFF the hot path.

## §4. Zero-loss buffering (operator mandate 2)

The GDF lane reuses the proven capture chain EXACTLY — no redesign (scope-lock §2/§4):

1. **Capture-at-receipt WAL** (`data/spill/gdf/`): the raw frame is appended BEFORE any
   parse or broadcast. A decode bug can never lose a frame — the bytes are already
   durable. Boot replays the WAL; replay is DEDUP-idempotent (feed-in-key DEDUP).
2. **Bounded ring** → **NDJSON spill** → **DLQ**: the QuestDB-outage absorb chain,
   unchanged from the Groww/Dhan lanes.

Arithmetic (ARITH, labeled): steady state ≈ 99 msg/s × ~300 B/frame ≈ **30 KB/s**. A
10× extreme burst ≈ 1,000 msg/s ≈ 300 KB/s. A 65,536-slot ring therefore absorbs ≈ 65 s
of the 10× burst before the first spill; beyond it, NDJSON spill + DLQ catch every
payload as recoverable text. WAL disk cost ≈ 0.7 GB/session raw (22,500 s × 30 KB/s);
retention/rotation follows the existing spill-partition sweep.

**Honest 100% claim (mandated §F wording):** 100% inside the tested envelope, with
ratcheted regression coverage: every GDF frame we RECEIVE is durably captured at receipt
(WAL-before-broadcast → ring → spill → DLQ, DEDUP-idempotent replay) — CAPTURE-complete.
Beyond the envelope, DLQ NDJSON catches every payload as recoverable text. NOT claimed:
frames the vendor conflated away before sending (the 1-second L1 ceiling, §13).

## §5. Per-frame OHLCV derivation law (operator mandate 4)

**Frame ladder (20 frames):** 1s, 2s, 3s, 4s, 5s, 6s, 7s, 8s, 9s, 10s, 11s, 12s, 13s,
14s, 15s, 30s, 1m, 3m, 5m, 15m.

| Rule | Law |
|---|---|
| Bucket | half-open `[anchor + k·F, anchor + (k+1)·F)`, anchor = **09:15:00 IST**; the final bucket of the session may be ragged at 15:30 (kept, flagged ragged) |
| O | first LTP in bucket |
| H / L | max / min LTP in bucket |
| C | last LTP in bucket |
| V | Σ of `TotalQtyTraded` **DELTAS** inside the bucket. A NEGATIVE delta = vendor counter reset → coded anomaly (`error!` with code) + counter; the bar NEVER goes negative (delta clamped to 0 for that step, anomaly counted) |
| OI | last `OpenInterest` in bucket |
| Empty bucket | **NO fabricated record-side bar.** Absence is recorded as absence; forward-fill happens ONLY at read/decision time (the §38.8 decision-freshness analog — record-completeness vs decision inputs stay distinct) |

**HONEST:** on a 1-second conflated L1 feed, H/L understate the true intra-second
extremes — the exchange may print higher/lower inside a second than the one conflated
snapshot shows. That is the VENDOR's ceiling [R#12]; every derived frame inherits it and
no surface may present these bars as exchange-true extremes (§13).

## §6. Connection lifecycle

| Step | Contract |
|---|---|
| Endpoint | SSM `/tickvault/<env>/gdf/endpoint` (String, `ws://host:port` — per-account, issued by GDF at enablement [R#4]; never hardcoded) |
| Credential | key = read-only SSM `Secret<String>` at `/tickvault/<env>/gdf/access-key` — **NEVER logged, NEVER in URLs**; held in memory only, zeroized on drop |
| Session lock | `instance-lock-gdf` SSM named lock acquired **FAIL-CLOSED before any connect** — GDF enforces 1 session/key and a 2nd login INVALIDATES the 1st [R#7]; lock refused ⇒ degrade (no GDF this boot) + page; NEVER fight the peer with reconnects |
| Auth | ONE in-band WS JSON frame `{"MessageType":"Authenticate","Password":"<key>"}` → `AuthenticateResult{Complete:true}` [R#6] — not REST |
| Connect probes | `GetLimitation` + `GetServerInfo` + `GetExchanges` in-band at every connect — the per-key entitlement oracle [R#23]; logged verbatim SANS key |
| Subscribe | 99 × SubscribeRealtime requests (one per symbol — no batch [R#12]), from the §8 morning set |
| Heartbeat | server-push `MessageType:"Echo"` every few seconds [R#10]; the client sends nothing; Echo silence = the feed-level stall watchdog signal |
| Reconnect | full re-auth + full re-subscribe (no resume protocol [R#11]); bounded expo ladder 5s → 60s; the instance lock is RE-CHECKED before every reconnect attempt |
| Audit | every disconnect/reconnect stamps `ws_event_audit` (`WsType::GdfFeed`, `feed='gdf'`) |

## §7. Identity (I-P1-11 + feed-in-key)

| Rule | Value |
|---|---|
| Primary id | `security_id` = GDF **TokenNumber** with namespace **bit 61** set (Groww uses bit 62; Dhan ids are < 2^32 — the three spaces are disjoint by construction) |
| TokenNumber facts | gated behind `detailedInfo=true` on GetInstruments and STRING-typed in the master (`"TokenNumber":"35089"` — LIVE-DOC, `docs/gdf-ref/06-instruments-and-identity.md`); "TokenNumber = the exchange's own token" remains **Assumed** [U-21] |
| Fallback | missing/non-numeric TokenNumber (observed `TokenNumber:null` in a SnapshotResult sample) → FNV-1a64 of `"<EXCHANGE>:<InstrumentIdentifier>"` with **bit 60** set + a build-time FAIL-CLOSED collision check over the day's watch set |
| Reversibility | the raw `InstrumentIdentifier` is persisted in `instrument_lifecycle.symbol_name` (feed='gdf') — every derived id maps back to the vendor string |
| Index identifiers | NSE_IDX identifiers are display names WITH spaces (`NIFTY 50`); BSE indices bare (`SENSEX`) — never guessed, resolved from the day's master |
| Rolling aliases | `-I/-II/-III` continuous-contract aliases are NEVER storage keys (they re-point across rolls) |
| Cross-feed joins | by ISIN / canonical index symbol / `(underlying, expiry, strike, side)` contract identity — NEVER by native id (I-P1-11; the futidx-4 §2 precedent) |
| Segment map | `NSE→NSE_EQ(1)`, `NSE_IDX/BSE_IDX→IDX_I(0)`, `NFO→NSE_FNO(2)`, `BSE→BSE_EQ(4)`, `BFO→BSE_FNO(8)`; `CDS`/`MCX`/`BSE_DEBT` EXCLUDED [R#3] |

## §8. Daily instrument rebuild (operator Quote 3 — automated, fail-loud)

Every trading morning (pre-market, before the 09:15 connect window) the 99-symbol
subscribe set is re-resolved from THAT DAY's in-band `GetInstruments` (NFO + NSE_IDX,
`detailedInfo=true`) — never from a stale cache as the primary source:

1. **Reference spot** = the PREVIOUS SESSION's NIFTY close **from our OWN stored data**
   (the `candles_1m`/`spot_1m_rest` close for the prior session). Justification: it is
   available pre-open, costs ZERO extra vendor calls, and is deterministic/replayable —
   a live pre-open quote would be none of the three.
2. **Strike window**: the 49 CE + 49 PE strikes of the CURRENT expiry nearest-centred on
   the reference spot (49 = the fixed half-chain per side; the grid step comes from the
   day's master, never hardcoded). Strikes listed OVERNIGHT by the exchange fall inside
   the window naturally — re-resolution picks them up.
3. **Expiry rollover**: on the morning AFTER expiry, the rebuild selects the next
   current expiry. NEVER an intraday resubscribe (the §36 never-roll precedent) — the
   expiring chain streams through its final session.
4. **Failure degrade**: bounded fail-loud retry; if the rebuild still fails, REUSE the
   previous day's persisted watch set (`data/gdf/gdf-watch-<date>.json`) + page the
   operator. A stale-but-valid set beats a dead feed; the page makes it loud.

**Test rows:** T-R1 strike-drift recentre — spot moved 400 pts overnight → the new set
recentres, the diff is logged symbol-by-symbol; T-R2 new-strike listing — an
overnight-listed strike inside the window enters the set; T-R3 expiry-morning rollover —
the set flips to the next expiry with zero intraday resubscribes the prior day; T-R4
rebuild failure — GetInstruments fails all retries → previous-day set reused + one page,
never a silent empty set.

## §9. RAM-first sizing + instance envelope (operator Quote 4 — the centerpiece)

**The LAW (daily-universe §7 Rule 6):** every derived frame for all ~99 instruments is
RAM-resident for the FULL trading day — indicators/decisions read RAM only. QuestDB is
persistence + audit + cold-boot rehydration, never the read path.

### §9.1 Bar-count arithmetic (ARITH; session = 09:15–15:30 = 22,500 s = 375 min)

| Frame | Bars/instrument/day | Frame | Bars/instrument/day |
|---|---|---|---|
| 1s | 22,500 | 9s | 2,500 |
| 2s | 11,250 | 10s | 2,250 |
| 3s | 7,500 | 11s | 2,046 |
| 4s | 5,625 | 12s | 1,875 |
| 5s | 4,500 | 13s | 1,731 |
| 6s | 3,750 | 14s | 1,608 |
| 7s | 3,215 | 15s | 1,500 |
| 8s | 2,813 | 30s | 750 |
| 1m | 375 | 3m | 125 |
| 5m | 75 | 15m | 25 |

Total ≈ **76,000 bars/instrument/day** → × 99 instruments ≈ **7.5M bars/day**
RAM-resident.

### §9.2 Cell layout — naive vs compact

| Layout | Bytes/bar | Today only | Today + yesterday |
|---|---|---|---|
| Naive struct (i64 ts + f64 OHLC ×4 + u64 vol/OI + flags, padded) | 72–100 B | ~550–760 MB | ~1.1–1.5 GB |
| **COMPACT fixed-point cell (chosen)** | **44–48 B** | **~360 MB** | **~720 MB** |

Compact cell: OHLC as **i64 paise ×4** (32 B, exact 2-dp fixed point — no float drift),
`u32` volume delta, `u32` OI, timestamp IMPLIED by the array index within the frame
(anchor + k·F — no stored ts), plus a presence bitmap (1 bit/bar per frame array) for
empty buckets. Greeks working sets ride the remaining headroom.

### §9.3 QuestDB DISK split (30 GB root — Quote 9 accepted size)

Persisting all 7.5M derived rows/day ≈ 0.3–0.5 GB/day is **UNTENABLE** on the 30 GB
root (weeks to disk pressure). Therefore:

- **Persist:** raw tick/L1 rows only (~2.2M rows/day: 99 × ~22,500 s) + `candles_1m`
  (~37K rows/day) + audit tables.
- **RAM-only:** all other 19 frames — derived in memory, REBUILT ON BOOT by replaying
  the day's stored ticks through the same aggregators (deterministic, DEDUP-safe).

### §9.4 Instance options — the choice is the OPERATOR's, not this doc's

| Option | RAM | Verdict here |
|---|---|---|
| (A) **t4g.medium** (current lock, §7 Quote 8) + compact cells | 4 GiB | **MARGINAL.** ~720 MB frame cells + app baseline + QuestDB 1g + OS leaves thin headroom above the kswapd floor; burstable CPU credits are un-validated for a 99 × 20-frame per-second workload. First live session is the measured gate: watch `tv_process_rss_bytes` / RESOURCE-02 / `mem_used_percent` / `CPUCreditBalance`. **This document does NOT conclude t4g.medium suffices.** |
| (B) **t4g.large** | 8 GiB | the standing rip-cord (daily-universe §7 Rule 2) |
| (C) latest-gen **m8g / r8g** (Mumbai) | 16+ GiB | under evaluation by the capacity-pack session, which OWNS the instance PR — referenced here, bill tables NOT duplicated |

The instance decision follows daily-universe §7 Mechanical Rule 1 in full: a dated
operator quote + the rule-file edits + the `instance_type_lock_guard.rs` ratchet +
terraform default — never this doc. **HONEST:** any upgrade beyond t4g.medium almost
certainly breaches the **< ₹1,000/month incl-GST hard target** (daily-universe §0
Quote 9) — that trade-off is the operator's to rule, not ours to assume.

## §10. Timeframe linkage — INTERFACE SPEC ONLY

The frame-ladder IMPLEMENTATION is the paused #1696 lane's territory — this design
defines only the OUTPUT contract its consumers (and this lane's aggregators) meet, so
the two lanes coordinate instead of colliding:

```rust
/// Sealed-bar event — emitted once per (frame, instrument) bucket close.
struct SealedFrameBar {
    frame_id: FrameId,            // 1s..15s, 30s, 1m, 3m, 5m, 15m
    security_id: u64,             // §7 identity (bit 61/60 namespaced)
    exchange_segment: ExchangeSegment,
    bucket_open_ts_us: i64,       // anchor + k·F, IST-derived, µs UTC
    o_paise: i64, h_paise: i64, l_paise: i64, c_paise: i64,
    volume_delta: u32,            // §5 delta law (never negative)
    oi_last: u32,
    present: bool,                // false = empty bucket (no fabricated bar)
    ragged: bool,                 // final 15:30 bucket flag
    feed: Feed,                   // Feed::Gdf
}
```

Consumers subscribe per frame id over a bounded in-process channel; sealed bars are
immutable once emitted. Nothing in this lane implements the ladder consumers.

## §11. Failure matrix

| # | Failure | Detection | Response | Loss? |
|---|---|---|---|---|
| 1 | Auth reject ("Invalid Password"…) | AuthenticateResult / diagnostic frame | coded error + bounded retry ladder + page; never a tight loop | none (no session) |
| 2 | Key already in use | GDF reject [R#7] / lock refused | `instance-lock-gdf` refused ⇒ degrade (no GDF this boot) + page; NEVER fight the peer | none (fail-closed) |
| 3 | Mid-day disconnect | read-loop EOF/error | ladder reconnect (5s→60s) + full re-subscribe; WAL replay covers the persist seam; `ws_event_audit` row | gap = outage window only, loudly counted |
| 4 | Per-symbol subscribe reject | per-request result frame | coded error naming the symbol; other 98 unaffected; page if > N symbols fail | that symbol only, visible |
| 5 | Stale feed (Echo silence) | feed-stall watchdog (no client ping exists) | stall alarm → reconnect ladder | bounded by watchdog window |
| 6 | Vendor-silence vs market-silence | per-tier liquidity baseline (index: never silent in-session; deep strikes: legitimately sparse) | index silence pages fast; deep-strike silence is counted, not paged (the §36.4 far-month precedent) | n/a — classification only |
| 7 | TotalQtyTraded counter reset | negative volume delta (§5) | coded anomaly + counter; bar clamped, never negative | none; anomaly visible |
| 8 | Malformed / non-JSON frame | tolerant decode [R#8] | WAL already holds the bytes; frame → DLQ + counter; never a panic | none (WAL + DLQ) |
| 9 | QuestDB outage | ILP write failure | ring → spill → DLQ absorbs; replay on recovery | none inside the envelope (§4) |
| 10 | Rebuild failures T-R1..T-R4 | §8 tests | recentre / include / rollover / previous-day-set + page | none; degrade is loud |

**Operator-mandate enforcement points:**

| Mandate | Enforced at |
|---|---|
| 1 — full field capture | §2 binding table (no field dropped) + tests T-W1..T-W3 |
| 2 — zero-loss buffering | §4 WAL-before-parse + ring/spill/DLQ + honest envelope |
| 3 — architecture | §3 pipeline, O(1)/msg + zero hot-path alloc per stage |
| 4 — per-frame OHLCV | §5 bucket/delta/no-fabrication laws + §10 sealed-bar contract |
| Quote 3 — daily rebuild | §8 automated fail-loud rebuild + T-R1..T-R4 |
| Quote 4 — RAM-first sizing | §9 residency law + compact cells + operator-owned instance call |

## §12. Day-0 trial checklist

1. Log the `GetLimitation` verdict VERBATIM (sans key) — the per-key entitlement oracle
   [R#23]: functions on/off, per-exchange symbol caps, DataDelay, history caps.
2. Measure delivery lag (`STT − LTT` + our receive-lag) against the Groww REST legs over
   the SAME minutes — the independent parity signal.
3. Measure per-tier second-coverage: index vs ATM strikes vs deep strikes — how many of
   the 22,500 session seconds actually carry an update per tier (feeds the §11 row-6
   silence classifier).
4. **Flag the cleartext `ws://` key transport to the operator** [R#5]: accepted for the
   trial ONLY; `wss://` availability is a sales question [U-2] — must be answered before
   any long-term contract.
5. Drill the instance lock: open a second session against the same key and verify the
   refusal path (degrade + page, no reconnect fight).

## §13. Honest envelope (mandatory — operator-charter §F / zero-loss charter §1)

> 100% inside the tested envelope, with ratcheted regression coverage: every GDF frame
> we RECEIVE is durably captured at receipt (WAL-before-broadcast → ring → NDJSON spill
> → DLQ, DEDUP-idempotent replay) — CAPTURE-complete. Beyond the envelope, DLQ NDJSON
> catches every payload as recoverable text.
>
> **NOT claimed:** sub-second fidelity (GDF conflates to ~1 update/sec/symbol — [R#12]);
> exchange-true highs/lows below the 1-second conflation ceiling (§5); market depth (L1
> only — [R#31]); tick-by-tick (REFUTED in the reference pack); millisecond timestamps
> (whole epoch seconds — [R#15]); vendor cadence under burst (day-0 probe [U-1]);
> whether the trial key is entitled for 99 NFO subscriptions (GetLimitation is the only
> authority [U-9]). The transport is plain `ws://` on the trial — the key crosses the
> network in cleartext [R#5], flagged in §12, accepted for the 1-week trial only. The
> firehose (mode A, StreamAllSymbols) is specced in the plan and DEFERRED here; nothing
> in this design forecloses it.
