# GDF 1-Second SubscribeRealtime — 99-Symbol Trial Architecture (2026-07-21)

> **Authority:** `.claude/rules/project/gdf-third-feed-scope-2026-07-13.md` (§9 = the
> 2026-07-21 operator directives, verbatim quotes) > `.claude/plans/active-plan-gdf-feed.md`
> (Status: APPROVED 2026-07-21) > this design record.
> **Ground truth for protocol facts:** `docs/gdf-ref/` (15-file pack, PR #1493) — citations
> `[R#n]` / `[U-n]` per its README legend. Evidence labels: **Verified / Assumed / Unknown**
> per `engineering-execution-standard-2026-06-26.md` §2.
> **Ownership:** per §9 Quote 5 the WAL/ring/capture architecture is Claude's responsibility
> to design and verify — this file is that record. All choices REUSE the existing resilience
> chain AS-IS (locked); nothing here redesigns it.

---

## §1. Transport & session

| Aspect | Design | Evidence |
|---|---|---|
| Endpoint | plain `ws://<host>:<port>` from SSM `/tickvault/<env>/gdf/endpoint` (issued per-account at enablement — never hardcoded) | [R#4]; §2 of the scope lock |
| TLS | NONE shown in the official corpus — the API key crosses the wire in cleartext. Accepted for the 1-week trial ONLY; `wss://` availability is a written sales question before any contract | [R#5]; [U-2] |
| Auth | ONE in-band JSON frame `{"MessageType":"Authenticate","Password":"<key>"}` → `AuthenticateResult{Complete:true}`. Key = SSM SecureString `/tickvault/<env>/gdf/access-key`, read into `Secret<String>`, memory-only, NEVER logged/URL-embedded; re-read (never cached past) an auth reject at ≥60s pacing | [R#6]; SDK verbatim |
| Session lock | `instance-lock-gdf` SSM named lock acquired BEFORE any connect (90s TTL + heartbeat, `dual-instance-lock-2026-07-04.md` machinery). GDF invalidates the previous session on a 2nd login — refused lock ⇒ degrade + page, NEVER fight the peer | [R#7] |
| Frame cap | lifted (official SDK uses 2^50); bounded by our own hard cap (256 MiB) for the huge `GetInstruments` frame | [R#26]; SDK ws.py |
| Reconnect | full re-auth + re-subscribe of all 99 symbols (no resume protocol); bounded expo ladder 5s→60s; lock re-checked each attempt; every disconnect stamps `ws_event_audit` (`WsType::GdfFeed`, feed='gdf'). Maintenance windows 02:00–02:30 / 08:00–08:45 IST treated as expected (no page storm) | [R#11]; [R#39, Assumed; U-18] |
| Heartbeat | server-push `MessageType:"Echo"` every few seconds; client sends nothing. Echo silence + zero rows in-session feeds the existing feed-level stall watchdog (kill + reconnect) | [R#10]; [U-12] cadence probe day 0 |

## §2. The 99-symbol daily resolution algorithm (pre-market, fail-closed)

Runs every trading morning BEFORE subscribing (per §9 Quote 4 — never a stale list):

1. Connect + Authenticate + `GetLimitation` (entitlement oracle — log verbatim, key
   redacted) [R#23].
2. `GetInstruments` in-band for NSE_IDX + NFO (the WS twin — the REST form puts the key in
   a URL and is FORBIDDEN) [R#29]/[R#35].
3. Resolve **NIFTY 50 spot**: the NSE_IDX row whose display identifier canonicalizes to
   `NIFTY` ("NIFTY 50" with the space — NOT `NIFTY-I`, that is the future) [R#28]. Take
   its `TokenNumber`.
4. Resolve the **current expiry**: from the NFO OPTIDX NIFTY rows, nearest expiry ≥ today
   (never-roll — the expiring day streams through its final session; tomorrow's build
   advances). Same selection discipline as the house `select_index_future_expiries` shape.
5. Build the **strike ladder**: reference price = previous close of the NIFTY spot row
   from the master (day-0 fallback; once live, yesterday's last captured spot LTP). Take
   the 49 CE + 49 PE strikes CENTERED on the ATM strike (24 below + ATM + 24 above at the
   listed strike step; if the master lists fewer on one side, extend the other side so the
   count stays 49+49 — the window is re-centered fresh every morning, so drift days
   self-correct next morning).
6. **Fail-closed:** if fewer than 99 symbols resolve (missing spot row, <49 strikes on a
   side after extension, null TokenNumbers unresolvable), the lane REFUSES to subscribe a
   partial silent set — it logs EVERY missing symbol BY NAME (typed GDF-INSTR error),
   pages once, and (trial-grade degrade) proceeds with the named-gap subset ONLY when the
   spot + ≥80% of strikes resolved; below that, no GDF this session. No stale-list reuse
   without a loud flag.
7. Persist the watch file `data/gdf/gdf-watch-<date>.json` (Groww watch-file pattern) +
   `instrument_lifecycle` rows feed='gdf' (SEBI master continuity).
8. Issue 99 × `SubscribeRealtime` (1 request/symbol — no batch form exists [R#12]);
   count acks/first-rows per symbol; a symbol with zero rows by 09:20 IST is a named-gap
   log line (update-driven silence for far strikes is legitimate — see §9).

## §3. Capture chain (REUSED AS-IS — the locked durable floor)

```
ws frame received
  └─► WAL append (data/spill/gdf/ — feed-parameterized ws_frame_spill) BEFORE any parse
        └─► parse (long-key RealtimeResult; all fields Option-al; text frames → typed
             NonJson arm, counted, never a panic)
              └─► bounded ring → (overflow) NDJSON spill → (poison) DLQ
                    └─► validator → shared pipeline: ticks feed='gdf' persist
                          ∥ frame-family aggregator (§5)
```

- **WAL-before-broadcast** is the capture guarantee: every frame RECEIVED is durable
  before any processing — boot replay is DEDUP-idempotent (feed-in-key keys).
- Ring/spill/DLQ constants and code are the existing chain, unchanged (scope-lock §2/§4).
- At 99 symbols × ~1 row/s the chain runs at ~1–2% of its designed load — the reuse is
  headroom, not a stretch (Verified arithmetic; the chain's envelope was sized for the
  retired ~770-SID Groww universe).

## §4. Identity

| Rule | Value |
|---|---|
| Primary id | `security_id` = GDF `TokenNumber` \| **bit 61** (GDF namespace; Groww index bit 62, Dhan < 2^32 — disjoint) |
| Fallback | missing/non-numeric TokenNumber (observed `TokenNumber:null` in one SnapshotResult sample) → FNV-1a64 of `"<EXCH>:<IDENTIFIER>"` \| **bit 60**, with a build-time fail-closed collision check over the day's 99-set (and the full master) |
| Reversibility | raw `InstrumentIdentifier` persisted in `instrument_lifecycle.symbol_name` (feed='gdf') — every id maps back |
| Cross-feed joins | by ISIN (stocks), canonical index symbol (indices), `(underlying, expiry, strike, opt_type)` contract identity (options) — **NEVER native id** (I-P1-11; a GDF token ≠ a Dhan SID ≠ a Groww token for the same contract) |
| Segment map | NSE_IDX→IDX_I(0), NFO→NSE_FNO(2) for this trial's set |

## §5. Per-frame OHLCV definition (the frame family)

**Frames:** F ∈ {1s, 2s, 3s, 4s, 5s, 6s, 7s, 8s, 9s, 10s, 11s, 12s, 13s, 14s, 15s, 30s,
1m, 3m, 5m, 15m} — per §9 Quote 3 (1s–15s "sequentially one by one" + 30s + the four
minute frames). *(+1d broker-pulled next pre-market — flagged coordinator assumption
pending operator veto; not built until confirmed.)*

**Bucketing:** bucket k of frame F = `[anchor + k·F, anchor + (k+1)·F)`, anchored at
**09:15:00 IST** (session open). Every F in the family divides evenly into the session
grid, so buckets align across frames (a 15m bar = 900 × 1s buckets exactly).

| Field | Rule |
|---|---|
| O | first LTP whose (whole-second) timestamp falls in the bucket |
| H / L | max / min LTP in the bucket |
| C | last LTP in the bucket |
| V | Σ LTQ deltas in the bucket (LTQ:0 quote-refresh rows contribute 0 — they update quote state, never fabricate a trade) |
| OI | last OI value seen in the bucket (options) |
| EMPTY bucket | **NO bar fabricated.** An interval with zero rows produces zero rows — absence is honest data (update-driven feed; far strikes legitimately silent). Forward-fill happens ONLY at decision/consumption time under the §38.8-class decision-freshness gate, never in storage. |

Timestamps are whole UTC epoch-seconds on the wire [R#15] — 1s frames are therefore the
FLOOR of achievable resolution, and a tick can land only on one whole-second boundary;
no sub-second placement is ever claimed.

## §6. RAM sizing (honesty — this is the instance-decision input)

- Session = 6h15m = 22,500 s. Bars/instrument/day across the family:
  22,500 × (1/1 + 1/2 + … + 1/15 + 1/30 + 1/60 + 1/180 + 1/300 + 1/900) ≈ 22,500 × 3.385
  ≈ **76,000 bars/instrument/day worst-case** (every bucket non-empty — the 1s frame alone
  is 22,500 of them).
- ×100 instruments (99 + margin) ≈ **7.6M bars/day** RAM-resident worst-case.
- Compact fixed-point cell: O/H/L/C as i64 paise + V/OI + ts ≈ **44–48 B/bar** ⇒
  ~**335–365 MB today-only** worst-case. With yesterday's set co-resident + indicator/
  strategy headroom + jemalloc/fragmentation slack, the honest planning peak is
  **~4.5–5.5 GB** (the house 2× yesterday+today convention plus working-set slack —
  Assumed until live-measured; the conflated feed's empty far-strike buckets will land
  the REAL number well below worst-case, but we size for worst-case).
- **Verdict: minimum 8 GiB host** for the trial capture (t4g.large class). The live
  t4g.medium (4 GiB, `daily-universe-scope-expansion-2026-05-27.md` §7 lock) does NOT fit
  the worst-case — the instance change is its OWN operator-gated PR per §7 Mechanical
  Rule 1 (dated quote + rule edits + ratchet update); this doc only records the finding.
  Alternative if the operator declines: cap the RAM-resident family to 1s/15s/1m/15m
  (rest built on read) — flagged, not chosen.

## §7. Tables & persistence budget

| Surface | Rule |
|---|---|
| Ticks | shared `ticks` table, `feed='gdf'`, DEDUP key `(ts, security_id, segment, capture_seq, feed)` — capture_seq disambiguates same-whole-second rows |
| Candles | shared `candles_*` family, `feed='gdf'`, feed-in-key DEDUP; the sub-minute frames persist to the same shared-candle discipline (frame column / per-frame table per the existing 21-TF aggregator contract — implementation PR decides within the shared-table lock; NO `gdf_*` parallel data tables) |
| 1s ingest budget | 99 rows/s tick ILP + ~99 rows/s 1s-bar seals ≈ ~200 rows/s vs the ~5K rows/s QuestDB ingest envelope — **~4%, trivially inside** (Verified arithmetic) |
| Greeks | NONE stored (§9(c) — BruteX owns greeks entirely; §37 read-only S3 seam) |
| Retention | day-partitioned, partition-manager registered, 90d class + S3 cold archival (30 GB root accepted per the 2026-07-19 ruling; 1s bars for 99 syms ≈ tens of MB/day — inside) |

## §8. Backtest / live parity (the slippage mission)

- **Replay source = OUR OWN captured stream** (WAL frames / ticks feed='gdf' with ARRIVAL
  timestamps), NEVER GDF `GetHistory` — no 1s bars exist there (minute floor; TICK depth
  ≈ 7 days observed [R#22-class, Assumed]) and its output is scope-locked to the
  cross-verify audit table only.
- Backtest replays the identical arrival-ordered rows the live lane saw, so backtest and
  live share EXACTLY the same information set — including the conflation blindness (§9):
  whatever intra-second movement live cannot see, backtest cannot see either.
  Symmetric blindness = honest parity.
- Fill model: worst-case-next-second — a signal evaluated on bucket k fills at bucket
  k+1's worst-case H (buy) / L (sell) for the option leg. This is the sub-minute analog
  of the operator's BruteX next-minute worst-case rule (§38.0 Context 3) and is the
  slippage-reduction measurement Quote 3 demands.
- No strategy code ships under this design (§28 boundary; decision-freshness gate class
  rules bind any future consumer).

## §9. Honest envelope (mandatory — no illusion)

- **Upstream is a 1-second CONFLATED L1 feed** [R#12]: ~1 update/sec/symbol full-image
  snapshot; intra-second trades are conflated by the vendor/exchange BEFORE reaching us —
  invisible to BOTH backtest and live, symmetrically. We never claim tick-by-tick.
- **Whole-second stamps** [R#15] — no sub-second precision anywhere; `LAG_FLOOR_MS_GDF =
  1000`.
- **Illiquid strikes tick sparsely** — the feed is update-driven; a far strike with no
  quote/trade change sends nothing. Empty buckets are recorded as absence, never
  fabricated (Rule 11 — no false-OK bars).
- **Plain `ws://`** — key in cleartext on the wire; accepted for the trial ONLY, flagged
  to the operator before any long-term contract [R#5]/[U-2].
- **Capture guarantee** = the §3 WAL-at-receipt chain: 100% inside the tested envelope,
  with ratcheted regression coverage — every frame RECEIVED is durably captured
  (WAL→ring→spill→DLQ, DEDUP-idempotent replay); beyond the envelope the DLQ catches
  every payload as recoverable text. NOT claimed: delivery of frames the vendor never
  sent, sub-second resolution, depth (L1 only [R#31]), or greeks.
- **Assumed/Unknown until day 0:** SubscribeRealtime cadence under 99 concurrent subs,
  per-key symbol caps (GetLimitation decides [R#22]/[R#23]), Echo cadence [U-12],
  timestamp epoch confirm per exchange [U-23], strike-cap subscribe-reject shape [U-14],
  RAM worst-case vs measured (the §6 gate).

## §10. Trial-Day-0 probe checklist (execute in order; evidence per item)

| # | Probe | Answers |
|---|---|---|
| 1 | SSM params set; lock acquired; Authenticate → Welcome | entitlement of the trial key exists |
| 2 | `GetLimitation` verbatim log (key redacted) | symbol caps, function flags (SubscribeRealtime enabled?), DataDelay, expiry [U-9] |
| 3 | `GetInstruments` NSE_IDX + NFO; run §2 resolution; watch file green, 99/99 named | identity + TokenNumber presence for the index [U-21] |
| 4 | Subscribe 99; count first-row latency per symbol; rows/s for 10 min | real cadence under 99 subs; conflation reality [U-1/U-17 class] |
| 5 | Timestamp semantics: LTT/STT vs wall clock on NIFTY spot | epoch-UTC + whole-second confirm [R#15]/[U-23] |
| 6 | Echo cadence measurement (30 min) | stall-watchdog threshold calibration [U-12] |
| 7 | Session-lock behavior: open a 2nd session deliberately OFF-HOURS | key-in-use reject text envelope [R#7]/[U-11]; confirm our back-off arm |
| 8 | `wss://` attempt against the issued endpoint | TLS availability [U-2] (also ask sales in writing) |
| 9 | Evening: `questdb_sql` count of ticks feed='gdf' + 1s-frame bars; RSS measurement vs the §6 budget | capture proof + the RAM gate |
| 10 | File every answer as dated notes in the rule file §9 / the plan | evidence discipline |

---

*Auto-driver line: Sir, instead of the party line shouting every fruit in the market, we
now ask supplier GDF to read out exactly 99 items — the NIFTY basket and its 98 nearest
option coupons — once every second. Every word is written into the permanent notebook the
moment it is heard, the list of 99 is re-picked fresh every morning, and the greek
calculations stay with the head office (BruteX). One honest warning stays on the wall:
GDF speaks once per second — anything that happened inside a second, nobody hears, not
live and not in the replay — which is exactly why the replay is fair.*
