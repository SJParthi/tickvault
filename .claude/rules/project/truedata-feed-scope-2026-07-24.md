# TrueData Fourth Feed — Live-Tick Feed Scope Extension (Operator Lock 2026-07-24)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §I > this file >
> `gdf-third-feed-scope-2026-07-13.md` (the feed-#3 pluggable template this
> mirrors) > `groww-second-feed-scope-2026-06-19.md` (the feed-#2 template) >
> `websocket-connection-scope-lock.md` (EXTENDED below) >
> `no-rest-except-live-feed-2026-06-27.md` > defaults.
> **Scope:** PERMANENT. Every Phase. Every PR. Every future Claude/Cowork session.
> **Operator-locked:** 2026-07-24 (verbatim quotes preserved below).
> **Status:** **This file is the SCOPE AUTHORIZATION RECORD** (operator quotes
> 2026-07-24) + the binding contract, TRIAL-FIRST. Code lands per
> `.claude/plans/active-plan-truedata-feed.md` (PR-A..PR-F). **Implementation
> PRs B–F may only start once that plan file's `**Status:**` flips to APPROVED
> by the operator** (design-first wall — the plan lands DRAFT in PR-A). The
> live TrueData key is a **trial** — everything ships DEFAULT-OFF and
> code-ready so day 0 of the trial is a config flip, not a build.
> **Ground truth:** `TrueData Market Data API Documentation v 2.6` +
> `TrueData TCP API Documentation v 2.3` + the TD Postman collections
> (operator-supplied 2026-07-24).
> **Extends (does NOT supersede):** the Groww feed contract
> (`groww-second-feed-scope-2026-06-19.md`) and the GDF feed contract
> (`gdf-third-feed-scope-2026-07-13.md`) are UNCHANGED. This file authorizes a
> SEPARATE, INDEPENDENT, **default-OFF** FOURTH market-data feed (TrueData) as
> **the intended live-tick source** under the same pluggable-feed contract.
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The verbatim operator demands (preserve exactly, do not paraphrase — typos included)

**Quote 1 (2026-07-24 — the directive):**
> "Dude see clearly note our plan is to do the trial version with true data but
> before that we need to be precisely ready with the truedata websocket live
> feed entirely dude … you need to upgrade the instance everything needs to be
> precisely and perfectly ready … go through their python code and all the
> attached docs … suppose if we subscribe 300 symbols what about our
> architecture ring buffer wal and everything … especially to not even miss
> even a single tick … entirely RUST O(1) … Even our db changes db design
> changes every changes are extremely needed."

**Quote 2 (2026-07-24 — ticks-only, derive all timeframes):**
> "see only websocket live feed alone dude okay? using ticks only we will always
> derive all our timeframes dude okay?"

**Quote 3 (2026-07-24 — RAM horizon + timeframe set):**
> "not 250 days bro atleast the current 7 to ten days minimum it should hold
> right bro see meanwhile we can have only one and 3 min max right dude 5 and 15
> min can be discarded right dude?"

**Quote 4 (2026-07-24 — RAM-first absolute, no DB in decisions):**
> "even through periodically we will move the data to questdb also but nowhere
> our strategies indicators trading decisions should never ever get the info
> anywhere anywhere from db dude because for us every nano second latency is
> extremely important dude … nowhere the latency should be impacted anywhere."

**Quote 5 (2026-07-24 — reversibility: switch off + shrink instance):**
> "suppsoe in the futute if i plan to avoid truedata means then eaisly it shodu
> lallwo me to switch off and reuce the isnatcne sioze also dud eokau? wil that
> work ddue?"

**Quote 6 (2026-07-24 — Monday trial readiness):**
> "see clelary ntoe if on modnay if we plan to sue theor trial then withotu a
> doubt it needs to be fully ready ddue okay?"

**Quote 7 (2026-07-24 — make the plan):**
> "yes dude make it as a detailed thorough plan"

---

## §1. The rule — one paragraph

Market-data feeds remain **pluggable**. A FOURTH feed provider is authorized:
**TrueData** (TrueData Markets / NSE-authorized realtime vendor), **feed
`'truedata'`, default OFF** (`feeds.truedata_enabled = false`,
`#[serde(default)]`). TrueData is implemented **natively in tickvault Rust** —
NOTHING is vendored from the TrueData Python/Node/.Net SDKs; they are protocol
REFERENCE ONLY (the Groww §32/§35 + GDF precedent). Transport is a
**`wss://push.truedata.in:<port>` WebSocket** carrying the TrueData realtime
protocol (JSON auth/subscribe control frames + binary tick frames; the
128-byte binary auth-response struct + per-tick binary payload per the v2.6
doc). Auth = user/password in the connect URL query
(`?user=<X>&password=<Y>`), both READ-ONLY from SSM `Secret<String>`
(`/tickvault/<env>/truedata/user`, `/tickvault/<env>/truedata/password`,
`/tickvault/<env>/truedata/endpoint` for host+port), NEVER logged. TrueData is
**the intended LIVE-TICK source** — its **tick stream drives the revived
tick→timeframe aggregator** so ticks derive ALL timeframes (Quote 2), reusing
the existing resilience architecture **AS-IS**: capture-at-receipt
(WAL-before-broadcast, feed-parameterized `ws_frame_spill`) → 200,000-seal ring
→ NDJSON spill → DLQ → the shared aggregator (its OWN instance,
`FeedStrategy::Truedata`) — and writes the **SAME shared tables** (`ticks`,
`candles_1m`, `instrument_lifecycle`, `ws_event_audit`, …) with every row
tagged `feed='truedata'` and `feed` in every DEDUP key (NO `truedata_*` parallel
data tables — the 2026-06-19 table-model decision binds feed #4 identically).
TrueData enforces **1 active session per key** server-side (a 2nd login is
refused "User Already Connected"), so a **1-session-per-key SSM named lock**
(`/tickvault/<env>/instance-lock-truedata`, the GDF/GROWW-SCALE machinery) gates
every TrueData connect, fail-closed. **RAM-FIRST is absolute** (Quote 4):
strategies/indicators/risk read ONLY from RAM; the QuestDB write is the async
cold path and NEVER touches a decision. TrueData drives NO strategy and places
NO order — capture + tick-derived timeframes + persistence ONLY during the trial
and until a fresh dated quote says otherwise.

---

## §2. The pluggable-feed contract (LOCKED)

| Aspect | Locked value |
|---|---|
| Feed id | `Feed::Truedata`, `as_str() = "truedata"`, `WsType::TruedataFeed`; added to `Feed::ALL` (every match stays exhaustive) |
| Default | **OFF** — `feeds.truedata_enabled = false`, `#[serde(default)]`; flipping the DEFAULT needs a fresh dated quote |
| Transport | `wss://push.truedata.in:<port>` (per-account host+port issued by TrueData; SANDBOX port 8086 / PROD port 8084 per the v2.6 doc — never hardcoded, from SSM), tokio-tungstenite (pinned 0.29.0). Read task drains socket → WAL → ring ONLY (doc point (f): slow drain overflows the receive buffer → server-side drops) |
| Auth | user + password in the connect URL query (`?user=…&password=…`); both SSM read-only `Secret<String>`; auth reply = 128-byte binary struct (MsgCode/Status/Message/Segment/MaxSymbols/Subscription/Validity) parsed O(1) fixed-offset. Key material NEVER logged, NEVER echoed |
| **Trade Binary layout (v2.6 doc p.16 — the EXACT tick struct, `feed #4` O(1) parser target)** | **90 fixed bytes, fixed offsets, `from_le_bytes` (endianness UNVERIFIED-LIVE — confirm on the sandbox probe): Msg Code(char)@0 · Symbol Id(i32)@1 · Time stamp(i32)@5 · LTP(f32)@9 · Volume(i32)@13 · ATP(f32)@17 · Tot Volume(i64)@21 · Open(f32)@29 · High(f32)@33 · Low(f32)@37 · Prev Close(f32)@41 · OI(i64)@45 · Prev OI(i64)@53 · Turnover(f64)@61 · OHL(char)@69 · Sequence No(i32)@70 · Bid(f32)@74 · Bid Qty(i32)@78 · Ask(f32)@82 · Ask Qty(i32)@86 = 90 bytes. NO loop, NO alloc, O(1). f32→f64 via `f32_to_f64_clean` (STORAGE-GAP-02). `Sequence No`@70 = the per-symbol loss-proof (doc (e)); `Bid/BidQty/Ask/AskQty`@74-89 = the L1 qty for worst-case-fill lot sizing (doc (g), both-active default frame)** |
| Session model | **1 active session per key** — a 2nd login is refused ("User Already Connected"). SSM named lock `instance-lock-truedata` acquired BEFORE any connect; refused → degrade (no TrueData this boot), page, never fight the peer with a reconnect storm. On a DIRTY disconnect leaving a server-side ghost session: graceful `{"method":"logout"}` on shutdown + the force-logout HTTP `api.truedata.in/logoutRequest?user=&password=&port=` then **mandatory 60 s wait** (v2.6 p.19) — this is a real ≥1-min feed-down window (honest loss, C6/C7) |
| Subscription | control frame per the v2.6 doc; subscribe TICK (+ Bid/Ask, both allowed by default — doc point (g), combined tick+quote frame gives L1 qty for worst-case-fill lot sizing); NEVER the 1min/5min bar subscription (we DERIVE all timeframes from ticks — Quote 2). Symbol set config-driven (300 → 500), bounded by the plan's `MaxSymbols` (from the binary auth reply) |
| Loss proof | **per-symbol Tick Sequence number** (doc point (e)): parser tracks seq; a gap fires `TD-GAP-01` naming the missing range — an INDEPENDENT vendor-counter proof of zero loss, on top of the WAL |
| Durable floor | capture-at-receipt: raw frame appended to the feed-parameterized WAL (`data/spill/truedata/`) BEFORE any parse/broadcast; ring→spill→DLQ unchanged; replay on boot, DEDUP-idempotent |
| Aggregator | shared engine CODE, OWN instance; `FeedStrategy::Truedata`. **CORRECTION (deep-research 2026-07-24, Verified in-repo):** the tick→timeframe aggregator (`MultiTfAggregator`/`AggregatorCell`/`FeedStrategy`/`LatePolicy`) was **HARD-DELETED 2026-07-17** (stage-3 dead-WS sweep, `candles/mod.rs`), NOT dormant — so this is a **REBUILD** (recoverable from git pre-sweep), materially larger than a "revive". `LiveCandleState`(128 B) + `TfIndex`(21-TF) + the seal ring SURVIVE and are reused |
| Live timeframe set | **1s + 1m + 3m core** (Quote 3); 5m/15m and intermediate-second frames are DISCARDABLE (config-gated). Backtest wide, run narrow. **CORRECTION (Verified):** `TfIndex` starts at **1m** (21 TFs, ordinal-locked to `[_;21]` arrays) — **1s does NOT exist** and MUST be a **SEPARATE 1s lane + a new `candles_1s` table**, never a 22nd ordinal (a 22nd variant SEMVER-breaks all 21 consumers). 1m/3m reuse the existing TfIndex |
| WAL per-feed path | **CORRECTION (Verified):** the WAL is TRANSPORT-typed (`WsType={LiveFeed,OrderUpdate}`), the record carries NO `feed` byte. TrueData needs a **per-feed WAL path `data/spill/truedata/` + a feed tag** added (PR-D) or frames replay ambiguously |
| Session-0 probe gate (HARD boot step) | before any `from_le_bytes` lands in prod, a **mandatory session-0 sandbox probe** confirms: (a) WSS trade default = **binary vs JSON** (the doc shows BOTH; if JSON, WSS breaks O(1) → use the TCP binary port 7070 fallback); (b) **endianness** (LE assumed, unconfirmed — X10 silent-corruption risk); (c) **Bid/Ask mode** (90-byte both-active vs 74-byte trade-only — len-dispatch); (d) parsed LTP within ±50% of the touchline snapshot (sanity). Fail-closed until confirmed |
| O(1) parse guarantee (transport choice) | **WSS+binary → tokio-tungstenite, 90-byte O(1) (preferred if probe confirms binary). WSS+JSON → REJECTED (alloc breaks O(1)). Fallback: TCP port 7070 = guaranteed binary 87-byte `Z`-framed (seq@10, no OHL tag) via a raw-TCP client.** The O(1) mandate is protected either way; the probe decides. Decision-path RAM READ is honestly **O(log ring)** (`RwLock`+binary-search, `spot_bar_store.rs`), NOT O(1) — parse/lookup/push/seq-check ARE O(1) |
| RAM horizon | current **7–10 days minimum** held in RAM (Quote 3) for the core set + indicator warmup; full multi-year history stays on disk/QuestDB only (physically cannot fit RAM, and live decisions never need it) |
| RAM-first (absolute) | decisions read RAM ONLY (Quote 4). QuestDB is the ASYNC cold path — persistence + audit + cold boot-rehydration; a DB stall/outage NEVER blocks or slows a decision. Banned-pattern scanner enforces no DB read in indicator/strategy/risk paths |
| Identity | **CORRECTION (design workflow 2026-07-24 — supersedes the shallower "security_id = TD SymbolID" reading):** the TrueData **SymbolID is LEARNED-AT-SUBSCRIBE** (you `addsymbol` by NAME; the numeric SymbolID first appears in the touchline reply; ticks then key on it) and is **session-scoped / potentially reassigned across reconnects (UNVERIFIED-LIVE)**. Therefore `security_id` MUST be a **STABLE id derived from the canonical NAME** — indices via `stable_index_security_id(name)`, contracts by `(underlying, expiry, strike, option_type)`, equities by ISIN — **exactly the Groww pattern, NEVER the volatile SymbolID**. SymbolID stays a **session routing key inside a `papaya` map only**, resolved to the stable id at parse time. Allocate TrueData a **DISTINCT namespace bit pair (bit 59 token / bit 58 FNV-fallback)** — NOT GDF's 61/60, NOT Groww's 62 — with a build-time pairwise-disjoint assertion (in-memory `security_id`-keyed structures are NOT feed-partitioned and would alias otherwise). Raw TD symbol string persisted in `instrument_lifecycle.symbol_name` (feed='truedata'). Cross-feed joins by ISIN / canonical index symbol / (underlying, expiry) — NEVER native id (I-P1-11 + futidx-4 §2) |
| Wire decode (compression) | **CORRECTION (design workflow):** the default WSS wire may be **LZ4-framed** (bar1min GZIP) — decode does a day-0 first-byte sniff (`{`=JSON else binary), **decompresses LZ4/GZIP into a PRE-ALLOCATED per-connection reusable scratch buffer** (NEVER per-frame alloc — preserves zero-alloc, must be re-DHAT'd), then total-no-panic `from_le_bytes`. Capture-at-receipt WALs the RAW (compressed) frame BEFORE decompress/parse |
| Volume width | **CORRECTION:** TrueData TTQ/TotVol is **i64**, but the shared `ParsedTick.volume` is `u32` (a liquid NIFTY-future/index day exceeds `u32::MAX`). TrueData decode SATURATES `u32::try_from(ttq).unwrap_or(u32::MAX)` + `tv_truedata_volume_saturated_total` + coded WARN naming the symbol; ban silent `as u32`. The proper `u64` widening of the SHARED `ParsedTick.volume` is a SEPARATELY-scoped cross-feed PR (Dhan/Groww regression + ILP DDL + `BufferedSeal ≤144B` re-assert + DHAT re-measure) — NOT bundled into a TrueData PR |
| Reconnect (no resume) | **CORRECTION:** TrueData has **NO resume protocol** — every disconnect = full re-auth + re-`addsymbol` of all ~300 symbols; there is a **daily maintenance blackout ~07:30–08:00 IST weekdays**; ticks the exchange published during the connect+auth+touchline window are **lost AT SOURCE** (the WAL cannot recover un-received frames — honest). A post-reconnect **backfill** (replay port 8082 / `getticks` REST) is RECORD-ONLY repair, NEVER a live-decision input (§38.8 freshness doctrine). A bounded **pre-touchline buffer** (`TRUEDATA_PREMAP_BUFFER`, few-thousand frames) holds ticks arriving before SymbolID is learned; overflow → the RAW frame is already WAL/DLQ-captured (never an unbounded RAM queue) |
| Persisted uniqueness | composite `(security_id, exchange_segment, feed)` per I-P1-11 + feed-in-key; every write carries `feed='truedata'` |
| Orders/strategy | NONE. TrueData is capture + tick-derived timeframes + persistence only. §28 indicators/strategies boundary untouched; `dry_run` stays true |

**Run modes:** `dhan_enabled` (retired live-WS) / `groww_enabled` (retired
live-WS) / `truedata_enabled` are independent booleans; any combination is
legal; TrueData OFF ⇒ byte-identical behavior to today.

---

## §3. Instance + reversibility contract (Quotes 1, 5, 6)

| Aspect | Locked value |
|---|---|
| Trial instance | UPGRADE from t4g.medium (4 GiB) → **m8g.large** (2 vCPU / 8 GiB fixed-perf Graviton4) for the 300→500-symbol tick load + 7–10-day RAM horizon. r8g.large (16 GiB) is the rip-cord if live RSS exceeds the m8g budget. Executed via the guarded instance workflow (the `downsize-instance.yml` pattern in reverse); EIP + EBS preserved; old volume snapshotted first |
| Reversibility (Quote 5) | switching TrueData OFF is a config flip / runtime toggle (default state); the RAM working set collapses to the REST-only footprint; the instance then downsizes m8g.large → t4g.medium via the SAME guarded workflow. Data already persisted stays (`feed='truedata'`). A feed OFF + a routine downsize — NEVER a code rip-out; the plumbing stays dormant |
| Instance-change protocol | any instance-type change is dated-quote-gated per `daily-universe-scope-expansion-2026-05-27.md` §7 Mechanical Rule 1 (this file's Quote 1 authorizes the trial upgrade; the reverse downsize is authorized by Quote 5). Update the instance_type_lock ratchet + terraform default + the upgrade script in the same PR |
| Cost honesty | m8g.large is ~2× the t4g.medium hourly rate; the bill delta + the sub-₹1,000 target interaction (`daily-universe-scope-expansion-2026-05-27.md` Quote 9) is recomputed in the plan and `aws-budget.md`. The trial is time-boxed; the downsize path (Quote 5) is the exit |

---

## §4. What is UNCHANGED (still locked)

- The Groww feed contract + the GDF feed contract + the Dhan REST stack —
  untouched.
- SAME shared tables + `feed` column; NO `truedata_*` parallel data tables;
  `feed` in every persisted DEDUP key.
- Composite `(security_id, exchange_segment)` uniqueness per I-P1-11 in every
  collection.
- The resilience chain (WAL/ring/spill/DLQ/aggregator) is REUSED, never
  redesigned.
- Indicators/strategies boundary (daily-universe §28); `dry_run` stays true; no
  order path.
- Design-first wall, serial-PR protocol, All-Green merge gate, 15+7 guarantee
  matrices.

---

## §5. What a PR that violates this lock looks like (REJECT)

- Starts ANY implementation PR (B–F) while
  `.claude/plans/active-plan-truedata-feed.md` `**Status:**` is still DRAFT (the
  design-first wall; PR-A lands the plan DRAFT — only the operator flips it to
  APPROVED).
- Ships `feeds.truedata_enabled = true` as the DEFAULT without a fresh dated
  quote.
- Creates any `truedata_*` parallel DATA table, writes a shared-table row
  without `feed='truedata'`, or omits `feed` from a DEDUP key.
- Vendors/imports ANY TrueData SDK code (Python/Node/.Net) or adds a sidecar for
  TrueData (native Rust only).
- Subscribes the TrueData 1min/5min BAR streams instead of deriving timeframes
  from ticks (Quote 2), or persists a bar the aggregator did not derive.
- Reads market data / a decision input from QuestDB in any indicator/strategy/
  risk path (Quote 4 — RAM-first absolute).
- Opens a TrueData connection without holding `instance-lock-truedata`, or
  fights the "User Already Connected" refusal with a reconnect storm.
- Puts parsing/logic/blocking on the socket READ task (doc point (f) — must
  drain → WAL → ring only).
- Drops the per-symbol Tick Sequence gap check (doc point (e)).
- Wires TrueData into any strategy/order/risk path.
- Gives TrueData a weaker durable floor than Groww/GDF (skips
  capture-at-receipt / WAL-before-broadcast).
- Logs the user/password, embeds them anywhere but the connect URL built from
  SSM, or stores them outside SSM + in-memory `Secret<String>`.

Any such PR MUST be rejected in review even if the operator approves verbally —
the operator must update this rule file FIRST with a dated quote.

---

## §6. Honest envelope (mandatory per `operator-charter-forever.md` §F)

> "100% inside the tested envelope, with ratcheted regression coverage: every
> TrueData frame we RECEIVE is durably captured at receipt (WAL-before-broadcast
> → 200,000-seal ring → NDJSON spill → DLQ, DEDUP-idempotent replay) —
> CAPTURE-complete — AND independently loss-proven against the vendor's per-symbol
> Tick Sequence number (doc point (e); a seq gap fires TD-GAP-01 with the missing
> range). NOT claimed: (a) that TrueData's retail realtime stream is true
> tick-by-tick every-trade — its cadence/field population/quantity semantics are
> UNVERIFIED-LIVE until the sandbox (8086) then prod (8084) trial; the first live
> session is the probe; (b) sub-second timestamp precision beyond what the v2.6
> binary payload actually carries; (c) that m8g.large fits the 300→500-symbol +
> 7–10-day RAM working set — the first live session MEASURES `tv_process_rss_bytes`
> and r8g.large (16 GiB) is the rip-cord; (d) any order/strategy behaviour —
> TrueData is capture + tick-derived timeframes + persistence only, dry_run stays
> true, §28 frozen. The socket read task does nothing but drain → WAL → ring
> (doc point (f)) so the receive buffer never overflows into server-side drops.
> RAM-first is absolute (Quote 4): a QuestDB stall/outage never blocks or slows a
> decision — the DB write is the async cold path."

---

## §7. Auto-driver / Insta-reel explanation

> Sir, the juice shop is auditioning a FOURTH supplier — TrueData — who promises
> the fastest price-shouting board yet, and even numbers every single shout
> (1, 2, 3…) so we can PROVE not one was missed. We build the whole listening
> desk now — a bigger notebook table (the instance upgrade), the tape-recorder-
> before-filing safety, the "one listener per password" lock — so the moment
> TrueData switches the line on Monday, we just pick up the phone. Two honest
> promises on the wall: (1) the boy on the phone does NOTHING but write down and
> drop each shout into the safety box — if he stops to do sums, the line clogs
> and shouts get lost, so he never does sums on the phone; (2) all our fast
> decisions read only from the whiteboard in front of us (RAM), never from the
> back-room filing cabinet (the database) — the cabinet is just for the record.
> And if you ever drop TrueData, one wall-switch turns him off and we shrink the
> table back down — no rebuilding.

---

## §8. Trigger (auto-loaded paths)

Always loaded. Activates on any session that:
- Edits `crates/common/src/feed.rs`, `crates/common/src/config.rs`
  (`FeedsConfig`/`TruedataConfig`), `crates/common/src/ws_event_types.rs`
- Adds/edits any file under `crates/*/src/**/truedata*` or containing
  `Feed::Truedata`, `truedata_enabled`, `TruedataFeed`, `push.truedata.in`,
  `instance-lock-truedata`, `/tickvault/<env>/truedata/`, `TD-GAP-01`,
  `TD-FEED-01`
- Edits `config/base.toml` `[feeds]` / `[feeds.truedata]` sections
- Adds any new `wss://` or `ws://` URL constant
- Edits `.claude/plans/active-plan-truedata-feed.md`
- Edits `deploy/aws/terraform/variables.tf` `instance_type` or the instance
  upgrade workflow/script (the trial upgrade + reverse downsize)
