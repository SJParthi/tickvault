# GDF 12 — Data Quality Assessment (vs Dhan-WS and Groww-WS)

> **Source:** synthesis over the whole pack; primary citations: https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/introduction/type-of-data-available/ · https://globaldatafeeds.in/nimbledatapro/ · nsearchives.nseindia.com Snapshot MDR vendor spec · tradingqna.com threads 62794 & 55045
> **Fetched:** 2026-07-13 · **Evidence tier:** SEARCH + CLIENT-LIB-SOURCE + ARITH (all timestamp decodes arithmetic-checked in the 2026-07-13 reconciliation pass). Dhan/Groww comparison rows come from tickvault's own verified in-repo knowledge (`docs/dhan-ref/`, `groww-second-feed-scope-2026-06-19.md`).
> **⚠ 2026-07-14 UPDATE:** source pages machine-fetched verbatim (HTTP 200) on 2026-07-14 (GitHub runner, `docs-fetch/gdf-2026-07-14` @ `7d354a8d`) — see the reconciliation section directly below. Cadence + candle-provenance + maintenance-window claims upgrade to **RUNNER-DOC (2026-07-14)**; U-18 CLOSED; U-7 closed at doc level.

---

## 2026-07-14 RUNNER-DOC reconciliation

### Confirmations (tier upgrades)

| §/claim | Verbatim proof | Source (RUNNER-DOC 2026-07-14) |
|---|---|---|
| §2 1-second L1 cadence | "Realtime data updating at 1 second frequency … This is L1 data with single best Bid & Ask details" · SubscribeRealtime "pushes market data every second (Bid/Ask/Trade)" · "1 second update frequency" · "L1 data @1sec frequency with single best bid/ask details" | https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/pricing-sales/api-pricing/ · …/introduction/type-of-data-available/ · …/introduction/introduction-to-apis/ · https://globaldatafeeds.in/apis/ |
| §2 the internally-contradictory marketing line | "True tick-by-tick data with time-stamp of 1 second" reproduced verbatim; same page's bandwidth math confirms the honest protocol fact: "our data updates every second and for all symbols simultaneously … 225*60=13500 messages per minute" = exactly 1 msg/sec/symbol | https://globaldatafeeds.in/nimbledatapro/ |
| §2 **conflation vendor-ADMITTED** | FAQ: "During live markets, **hundreds of trades can occur every second**" — the vendor's own statement that trade count exceeds the 1/sec update cadence; conflation is therefore first-party-acknowledged, no longer only our NSE-spec inference | …/faqs/faqs/ |
| §2/U-7 historical tick granularity | type-of-data-available backfill tables: Periodicity **"Tick — Every 1 Sec"** (× 1 calendar week, every segment) — the tick archive IS documented as the 1-second stream. **U-7 CLOSED at doc level** (live spot-check remains trivial trial-day work) | …/introduction/type-of-data-available/ |
| §3 candles vendor-built — **was Assumed, now vendor-CONFIRMED** | FAQ verbatim: "**each data vendor constructs intraday candles using their own tick aggregation and filtering logic**"; "Open, High, and Low values may vary slightly between platforms, even though the Close price and total traded Volume usually match"; "the total traded volume we receive matches the exchange day volume" | …/faqs/faqs/ |
| §3 self-consistency claim | "Candles once formed do not change after relogin / next day" + "Data with no gaps … backfill is entirely automatic" | https://globaldatafeeds.in/nimbledatapro/ |
| §4 vendor latency/uptime marketing | "Low latency realtime data with ultrafast delivery", "Zero delay realtime data. Compare it with your broker's terminal anytime", "uptime of 99.995%, we handle 50 million data requests daily" — still marketing, still ZERO third-party measurements (U-19 open) | https://globaldatafeeds.in/nimbledatapro/ · …/introduction/introduction-to-apis/ |
| §5 session-boundary row — **Assumed → RUNNER-DOC, U-18 CLOSED** | FAQ verbatim: server maintenance "02:00:00 To 02:30:00 AM (MidNight) / 08:00:00 To 08:30:00 AM (Morning) / Some times morning activity (BOD Process) till 08:45:00 AM"; during windows the server answers "**Connection refused**"; "if you are connecting and setting up any tasks/cron jobs, etc., please connect after 8:45AM" | …/faqs/faqs/ |
| §5 FIX row / U-4 | /apis/ FIX section verbatim: "Delivers tick-by-tick market data with minimal latency" — marketing-only; NO FIX documentation page exists anywhere in the 215-page crawl (only family with no docs link); **U-4 stays open**, sales contact is the route | https://globaldatafeeds.in/apis/ |
| §5 L1/no-depth row | Every data description remains "single best Bid & Ask"; no depth product appears on any fetched page — L1-only re-confirmed | api-pricing · type-of-data-available · /apis/ |

### New facts affecting the quality verdict

1. **Cross-verify anchors, vendor-blessed:** per the FAQ, GDF's own position is that **Close + total traded Volume** are the cross-platform-stable fields while **O/H/L legitimately differ** between vendors ("common industry behavior", citing Zerodha's chart-difference note). Any tickvault↔GDF candle comparison should therefore weight Close/Volume as the parity anchors and treat O/H/L deltas as expected sampling noise — exactly the §3 "exact-match parity must NOT be expected" stance, now first-party. — RUNNER-DOC (2026-07-14, …/faqs/faqs/)
2. **Operational collision — tickvault boot vs GDF BOD window:** the AWS box auto-starts 08:30 IST (`daily-universe-scope-expansion-2026-05-27.md` §7) — INSIDE GDF's 08:00–08:30/08:45 BOD maintenance window. Per the vendor's own advice, any future GDF connect at boot must defer to **≥ 08:45 IST** (expect "Connection refused" before that; do not classify it as an outage or burn reconnect-quota against it). Complements the plan's Trial-Day-0 runbook. — RUNNER-DOC (2026-07-14, …/faqs/faqs/)
3. **Reconnect/resubscribe quota cost:** the FAQ documents a per-key Request-per-hour limit (examples 1800/3600/7200) where subscribe, unsubscribe AND re-subscribe each consume quota — a full-universe resubscribe after a disconnect is quota-priced. Reinforces the §5 "re-subs consume quota" row with first-party semantics (full detail in file 01 reconciliation). — RUNNER-DOC (2026-07-14, …/faqs/faqs/)
4. **Client-host guidance:** vendor warns against burstable cloud instances (AWS T2/T3) for consistent market-data workloads — tickvault's r8g.large (non-burstable Graviton4) complies. — RUNNER-DOC (2026-07-14, …/faqs/faqs/)

### Drift / refutations

- **None.** No 2026-07-13 claim in this file was contradicted; the 2026-07-14 corpus only strengthened tiers. The third-party "millisecond timestamps for GFDL" refutation stands (still zero ms values on any fetched wire sample).

### U-row status after this pass

| U-row | Verdict |
|---|---|
| **U-18** (maintenance windows) | **CLOSED** — stated verbatim in the FAQ (02:00–02:30 + 08:00–08:30, BOD to 08:45, "Connection refused", connect after 08:45). |
| **U-7** (historical tick granularity) | **CLOSED at doc level** — "Tick — Every 1 Sec" is the documented archive cadence; trial-day spot-check remains. |
| **U-4** (FIX TBT/depth) | **OPEN** — marketing claim re-confirmed verbatim; no FIX docs exist; sales route. |
| U-6 (bar stamp), U-17 (burst cadence/lag distribution), U-19 (reliability), U-23 (per-exchange epoch) | **OPEN** — live-trial questions, unchanged. |

---

## 1. Timestamps — epoch-seconds, TRUE UTC base, no milliseconds anywhere

| Finding | Proof |
|---|---|
| WS timestamps (`LastTradeTime`, `ServerTime`, Greeks `Timestamp`, `From`/`To`) are UNIX epoch **SECONDS** on the standard UTC epoch | Doc verbatim: "'1658138400' (18-07-2022 15:30:00) … seconds since Epoch time (i.e. 1st January 1970)". ARITH: 1658138400 UTC = 10:00:00 UTC = **15:30:00 IST exactly** (NSE close). Second independent example: 1567655100 = 09:15:00 IST. All in-session samples land inside NSE hours ONLY under the true-UTC reading |
| **NOT the Dhan IST-shifted pseudo-epoch** | under an IST-shift these samples would decode to nonsense wall times |
| No sub-second precision exists | grep of wsgfdl-py 1.3.5 README for 13-digit values on WS: ZERO hits; REST ms values ALWAYS end `000`. GDF marketing itself: "time-stamp of 1 second" |
| Third-party "millisecond timestamps for GFDL" claims are FALSE | refuted against every official wire sample — do not cite |
| Both exchange + server time per tick | `ServerTime − LastTradeTime` = free per-tick lag probe (1s skew seen in samples); live distribution **Unknown** (U-17) |
| Live epoch-UTC confirmation on ALL exchanges | verified by doc arithmetic only — probe live ticks vs wall clock on every entitled exchange (U-23) |

## 2. Cadence — 1-second conflated L1, NOT tick-by-tick

- GDF's own docs: "Realtime data updating at 1 second frequency … L1 data with single best Bid & Ask details." Official SDK: "client will receive the data with 1 second frequency."
- Distribution reality (adversarially confirmed): NSE's non-colo vendor broadcast is ITSELF ~1s snapshots (NSE spec: "MARKET FEED … (Realtime Snapshot)"; TBT exists only at NSE colo — "not available at TAP Server or through DotEx for further broadcast"). NIFTY futures alone runs 200–300 trades/sec — impossible over any 1-push/sec retail feed. **Dhan / Groww / GDF retail websockets are ALL conflated-snapshot derivatives.**
- "True tick-by-tick data with time-stamp of 1 second" (GDF marketing) is internally contradictory; the honest protocol fact is 1 full-image update/sec/symbol. The FIX tier's "tick-by-tick" nature is **Unknown** (U-4).
- Historical TICK rows are 1–2 s apart in official dumps — consistent with the archive being the 1s stream (U-7). **2026-07-14: doc-confirmed** — type-of-data-available lists Periodicity "Tick — Every 1 Sec"; U-7 closed at doc level (see reconciliation §).
- **Implication:** GDF does NOT fix the Dhan "conflated/sampled feed" complaint. It is arguably MORE REGULAR (fixed 1s cadence claimed for every symbol every second — burst behavior unverified, U-17) but strictly not richer.

## 3. Candle provenance — vendor-built (Assumed), self-consistent, not exchange-parity

- No claim of exchange-official candles exists anywhere. GDF is an L1 REALTIME vendor; its candles are built by GDF from its own 1s feed — **Assumed** (U-6/U-7 class). **2026-07-14: vendor-CONFIRMED** — FAQ: "each data vendor constructs intraday candles using their own tick aggregation and filtering logic"; O/H/L may differ cross-platform, Close + day Volume match the exchange (see reconciliation §).
- GDF's actual claim is SELF-consistency: "candles do not change once formed … no signal mismatch after backfill or next day; identical data delivered to all users" — live == their own backfill, NOT exchange-parity.
- Exchange-official EOD IS available separately: `GetBhavCopyCM`/`GetBhavCopyFO` on docs.globaldatafeeds.in (09 §6).
- Consequence for any cross-verify: comparing tickvault candles vs GDF candles tests "our capture vs GDF capture". High/Low on 1s-sampled candles carry the same sampling-error class as Dhan-derived candles; exact-match parity must NOT be expected.

## 4. Latency & reliability

| Item | Finding |
|---|---|
| Vendor claims | "Low latency realtime data with ultrafast delivery", "zero delay", "entirely automatic backfill, no gaps" — marketing, unmeasured |
| Third-party measurements | **NONE found** (U-19). No published latency numbers, no independent benchmark |
| Forum signal | traderji/tradingqna threads unreachable from the research env; surfaced snippets are GDF-favourable testimonial content — low-signal. Genuinely unassessed |
| Built-in probe | `ServerTime − LastTradeTime` per tick makes lag self-measurable from day 1 of any trial |

## 5. Comparison table — GDF vs Dhan-WS vs Groww-WS

| Dimension | **GDF WS** | **Dhan WS** (feed #1 heritage) | **Groww WS** (feed #2) |
|---|---|---|---|
| Update cadence | fixed ~1/sec/symbol, full-image L1 | ~1s-class conflated broadcast (observed ~2–4 msgs/sec/SID) | conflated NATS push, LTP-only messages |
| Timestamp precision | whole SECONDS | whole seconds (LTT) | **milliseconds** (`tsInMillis`) |
| Timestamp epoch | true UTC epoch (ARITH-verified) | **IST-shifted** epoch seconds (subtract 19800 for UTC) | standard epoch ms |
| Suggested lag floor (scoreboard class) | **1000 ms** (`LAG_FLOOR_MS_GDF` = second-resolution feed, same floor class as Dhan) | 1000 ms | 1 ms |
| Wire format | JSON text frames, MessageType-tagged | binary fixed-offset packets (8-byte header) | NATS-over-WS + protobuf |
| Per-tick content | LTP/LTQ/ATP + best bid/ask + OHL + prev-close + Vol + Value + **OI inline** + PreOpen + PriceChange | Quote(50B): LTP/LTQ/ATP/vol/buy-sell qty/OHLC; **OI = separate code-5 packet** | LTP + ts only |
| Depth | NONE (L1) | 5-level in Full packet (162B; depth feeds scope-locked OFF in tickvault) | none |
| Prev-day close | inline every tick (`Close`) | Quote bytes 38-41 / code-6 for IDX_I | absent |
| Official candle source | SubscribeSnapshot stream + GetHistory (vendor-built, Assumed) | REST intraday/historical (KEEP-class narrow grants only) | none (FORBIDDEN — §33 live-only) |
| Session/auth model | 1 WS session/key, key-as-Password frame, **new connection kills old** | token in URL query; ≤5 conns/account (tickvault locked to 2) | daily-minted token via SSM; 1 conn (locked) |
| TLS | **plaintext ws:// only observed** (U-2) | wss:// | wss:// |
| Heartbeat | server Echo push (passive) | server WS ping ~10s | NATS PING/PONG |
| Reconnect | client-owned; resubscribe-all; re-subs consume quota | client ladder + SubscribeRxGuard | sidecar/native ladder |
| Session boundary | maintenance windows 02:00–02:30 & 08:00–08:30 IST, BOD sometimes to 08:45 ("Connection refused"; connect ≥08:45) — **RUNNER-DOC 2026-07-14, U-18 CLOSED** | — | ~06:00 IST daily token reset |
| Identity | string identifiers + exchange TokenNumber + ISIN | numeric SecurityId + segment | numeric exchange_token |
| Independent measurements | none (U-19) | in-house measured daily | in-house measured daily |

## 6. Verdict (research-only)

- GDF solves NEITHER of the two Dhan pain points (conflation, second-resolution stamps). Groww's ms-precision feed remains strictly better on both axes.
- GDF's genuine strengths: uniform documented 1s cadence for all symbols; per-tick lag observability (dual timestamps); inline OI + VWAP + PreOpen; a real historical API with sane UTC epochs and generous quotas — useful as an INDEPENDENT third source / cross-verify anchor, with the vendor-built-candle caveat.
- **StreamAllSymbols firehose (wire LIVE-DOC-captured 2026-07-13 — 10):** a documented whole-entitled-universe push mode with zero subscribe management — unique among the three feeds (Dhan and Groww are both per-symbol-subscribe). Same 1s L1 quality class and epoch-seconds stamps (STT−LTT=1s in samples); a distinct Batch/abbreviated-key dialect; delivers untraded symbols too (zero-OHLC rows). Cadence/bandwidth unmeasured (U-1 residual).
- Everything quality-critical that remains open is a live-trial question: U-6 (bar stamp), U-7 (tick history granularity), U-17 (burst cadence + lag distribution), U-19 (reliability), U-23 (epoch confirmation per exchange).

## What this prevents

- Buying GDF expecting millisecond or tick-by-tick data (it is a 1s L1 feed by its own documentation).
- Setting a sub-second lag floor for GDF in any scoreboard math.
- Expecting exact candle parity vs exchange-official or tick-built candles.
