# GDF 12 — Data Quality Assessment (vs Dhan-WS and Groww-WS)

> **Source:** synthesis over the whole pack; primary citations: https://globaldatafeeds.in/global-datafeeds-apis/global-datafeeds-apis/introduction/type-of-data-available/ · https://globaldatafeeds.in/nimbledatapro/ · nsearchives.nseindia.com Snapshot MDR vendor spec · tradingqna.com threads 62794 & 55045
> **Fetched:** 2026-07-13 · **Evidence tier:** SEARCH + CLIENT-LIB-SOURCE + ARITH (all timestamp decodes arithmetic-checked in the 2026-07-13 reconciliation pass). Dhan/Groww comparison rows come from tickvault's own verified in-repo knowledge (`docs/dhan-ref/`, `groww-second-feed-scope-2026-06-19.md`).

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
- Historical TICK rows are 1–2 s apart in official dumps — consistent with the archive being the 1s stream (U-7).
- **Implication:** GDF does NOT fix the Dhan "conflated/sampled feed" complaint. It is arguably MORE REGULAR (fixed 1s cadence claimed for every symbol every second — burst behavior unverified, U-17) but strictly not richer.

## 3. Candle provenance — vendor-built (Assumed), self-consistent, not exchange-parity

- No claim of exchange-official candles exists anywhere. GDF is an L1 REALTIME vendor; its candles are built by GDF from its own 1s feed — **Assumed** (U-6/U-7 class).
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
| Session boundary | maintenance windows 02:00–02:30 & 08:00–08:45 IST (Assumed) | — | ~06:00 IST daily token reset |
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
