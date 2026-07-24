# TrueData Fourth Feed — Live-Tick Feed Scope Extension (Operator Lock 2026-07-24)

> **⚠ EVIDENCE CORRECTION 2026-07-24 (same day) — READ §9 FIRST.** The §1/§2
> transport rows below were written from an **email-gated vendor PDF ("v2.6" /
> "TCP API v2.3") that is NOT in this repository**. A three-agent audit against
> the IN-REPO reference pack (`docs/broker-ref-upload-2026-07-15/truedata/`,
> 15 files + `catalog-truedata.md`) found **seven claims with ZERO in-repo
> support or direct contradiction** — the 90-byte binary tick struct, the
> 128-byte binary auth reply, the TCP port 7070 binary fallback, the prod port
> default, the 60-second force-logout wait, the weekday-only maintenance
> window, and the "intended live-TICK source" granularity framing. **§9 (the
> dated correction section at the end of this file) SUPERSEDES those rows.**
> Where §2 and §9 conflict, **§9 wins** (house banner-wins convention). No
> operator scope changed — this is an evidence correction, not a scope change;
> the §0 operator quotes are untouched.
>
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

---

## §9. EVIDENCE CORRECTION — 2026-07-24 (same day, in-repo doc audit)

> **What this is:** a three-agent audit of the IN-REPO TrueData reference pack
> (`docs/broker-ref-upload-2026-07-15/truedata/` — 15 files — plus
> `docs/broker-ref-upload-2026-07-15/catalog-truedata.md`), which the original
> drafting of §1/§2 did NOT read. Every row below carries a verbatim quote +
> `file:line`. **This section SUPERSEDES the conflicting §1/§2 rows.**
> **No operator scope is changed** — the §0 quotes stand verbatim and
> untouched. This corrects OUR factual claims, not the operator's authorization.

### §9.1 The seven corrections (each supersedes a §1/§2 row)

| # | §2 row said (PDF-sourced, NOT in repo) | In-repo evidence says | Status |
|---|---|---|---|
| 1 | **90-byte binary Trade struct**, fixed offsets, `from_le_bytes` | "**Raw messages are JSON.**" (`truedata/03-realtime-websocket.md:52`); "JSON, **LZ4-compressed by default**" (`catalog-truedata.md` Cat-1); "all websocket messages are compressed for performance improvements by default" (`truedata/02-authentication-connection.md:129`) | **RETRACTED — unsupported.** Wire is LZ4-compressed JSON. |
| 2 | **TCP port 7070** = guaranteed binary fallback | `grep -rn "7070" docs/…/truedata/` → **zero hits**. Documented ports: 8082 (prod default), 8083 (migration), 8084 (alt prod, only if assigned), 8086 (sandbox), 8088 (pushbeta), 9084 (full feed), replay 8082 — **all the same JSON WS protocol** (`truedata/02-authentication-connection.md:29,33-41`) | **RETRACTED — port does not exist in evidence.** |
| 3 | Auth reply = **128-byte binary struct** (MsgCode/Status/…/MaxSymbols) | "the server sends a welcome/handshake message whose **JSON contains the text `TrueData`**" (`truedata/03-realtime-websocket.md:22`) | **RETRACTED.** Welcome is JSON; `MaxSymbols`-in-auth-reply unsupported (symbol cap is a plan-side limit). |
| 4 | Transport: "SANDBOX 8086 / **PROD 8084**" | prod default is **8082**; "8084 only if specified to you"; 8086 is sandbox (`truedata/02-authentication-connection.md:33-41`) | **CORRECTED** — prod default 8082 (never hardcoded; SSM-supplied, TrueData-assigned). |
| 5 | Ghost session: "**mandatory 60 s wait**" | force-logout `https://api.truedata.in/logoutRequest?user=…&password=…&port=…` then "**Wait ~5 minutes**, then log in again" (`truedata/02-authentication-connection.md:105-107`; `truedata/11-limits-errors-faq.md:45-48`) | **CORRECTED — ~5 minutes (5× longer).** The reconnect state machine's `CoolingDown` MUST use ~5 min, not 60 s. |
| 6 | Maintenance blackout "~07:30–08:00 IST **weekdays**" | weekdays **07:30–08:00**; **Sat/Sun 07:30–10:30** (`truedata/11-limits-errors-faq.md:66-67`) | **EXTENDED** — weekend window added. |
| 7 | "the intended **LIVE-TICK** source" / "not even miss a single tick" framing | **"CONFLATED ~1-second L1 snapshot stream, NOT true tick-by-tick"** (`catalog-truedata.md:20`); vendor grid: "L1 data **@1sec frequency**" (`truedata/01-overview.md:46`); NSE: "Tick-by-tick data is available **only at NSE co-lo servers** … not available at TAP Server or through DotEx for further broadcast" (`catalog-truedata.md:39`) | **CORRECTED — see §9.4.** |

### §9.2 The wire, as evidenced (binding until the sandbox probe says otherwise)

| Aspect | Evidenced value |
|---|---|
| Frames | **JSON text, LZ4-compressed by default** (SDKs decompress transparently) |
| Auth | user+password as **connect-URL query params** (`?user=…&password=…`) — no separate token; SSM `Secret<String>`, NEVER logged (unchanged) |
| Handshake | welcome JSON containing the literal `TrueData`; subscribe AFTER it |
| Ops | `addSymbol` / `removeSymbol` / `getMarketStatus` / `logout` as JSON request text on the open socket; symbols by **NAME** (`truedata/03-realtime-websocket.md:28,44`). **Exact raw JSON request keys are in the gated PDF — Unknown in-repo** (`truedata/00-INDEX.md:35`) |
| Trade message | ordered **19-field** list, frame carries the token `trade`: Symbol ID, Date-Time, LTP, LTQ, ATP, TTQ, Open, High, Low, Prev Close, OI, Prev OI, Turnover, Special Tag (`OHL`/`H`/`L`/``), **Tick Sequence No**, Bid, Bid Qty, Ask, Ask Qty (`truedata/03-realtime-websocket.md:101-106`). **Array-vs-object shape = Assumed (positional); settled by the day-0 sandbox capture.** |
| Timestamp precision | **Unknown** — no doc states ms vs whole seconds (`catalog-truedata.md` headline row 11). Day-0 probe settles it. |
| Bid/Ask | L1 inline in fields 16–19 **and** a separate `bidask` stream; if not entitled, **zeros are appended "to keep the structure intact"** (`truedata/03-realtime-websocket.md:139`) ⇒ **field count is stable; the 74-vs-90-byte length dispatch is UNNECESSARY** |
| Other message types the client MUST tolerate | `touchline`, `bidask`, `bidaskL2` (BSE L2 top-5), `bar` (1m/5m), `greeks`, `marketstatus`, `heartbeat` (`truedata/03-realtime-websocket.md:65-76`) — §2 named only trade+heartbeat |
| Heartbeat | server-push **every 5 seconds**, client sends nothing (`truedata/03-realtime-websocket.md:80`) |
| Session | "You can Connect only from **1 place at a time (1 login instance)**" (`truedata/11-limits-errors-faq.md:26`) — the SSM `instance-lock-truedata` design STANDS |
| Live rate limit | "For the Real-Time API, there is **no such limit**" (`truedata/11-limits-errors-faq.md:29`) |
| Symbol cap | "As per your Plan's symbols limit" — numeric value **Unknown** (`truedata/03-realtime-websocket.md:28`) |

### §9.3 O(1) — the corrected, honest complexity claim (supersedes the §2 "O(1) parse guarantee" row)

The §2 row asserted "WSS+JSON → REJECTED (alloc breaks O(1))" and promised a
binary fallback. With JSON as the ONLY evidenced transport, that promise is
void. The corrected, binding claim:

| Operation | Honest complexity | Note |
|---|---|---|
| Per-field decode | **NOT O(1)** — a JSON payload must be scanned; fixed byte offsets are impossible | stated plainly; never relabelled |
| Per frame | **O(frame bytes), fixed upper bound, ZERO heap allocation** | reusable per-connection scratch buffers; hand-rolled positional scan; NO `serde_json::Value`, NO `String`, NO per-frame `Vec`; DHAT-gated |
| Per tick, amortized | **amortized constant** | frame size is bounded by the fixed 19-field shape |
| SymbolID → stable id | **O(1)** avg | `papaya` lock-free map |
| Seq-gap check | **O(1)** | per-symbol last-seq compare |
| Persisted dedup / uniqueness | **O(1)** | QuestDB DEDUP UPSERT KEYS incl. `feed` |
| RAM decision read | **O(log ring)** | `RwLock` + binary search (`spot_bar_store.rs`) — unchanged, still flagged |

**Binding wording rule:** no PR, commit, doc, or operator-facing message may
describe the TrueData parse as "O(1)". The sanctioned phrase is
**"zero-allocation, amortized-constant per tick (O(frame bytes), fixed
bound)"**. Claiming fixed-offset O(1) on the JSON wire is a REJECT (§5).

### §9.4 Granularity — the corrected honest envelope (supersedes the §6 "live-tick" framing)

> TrueData's realtime WebSocket is **NOT true tick-by-tick**. The vendor's own
> feature grid states "L1 data @1sec frequency" (`truedata/01-overview.md:46`),
> and NSE states TBT "is available only at NSE co-lo servers … not available at
> TAP Server or through DotEx for further broadcast" (`catalog-truedata.md:39`)
> — **no redistributable Indian vendor feed, from ANY provider, can deliver
> every exchange trade.** The verdict is **NSE-F&O-anchored**; NSE EQ, NSE
> indices, BSE and MCX are **Inferred** as the same vendor-feed class and are
> per-segment **UNVERIFIED** (`catalog-truedata.md:21-24`).
>
> **What we CAN guarantee:** CAPTURE-completeness of every message we RECEIVE —
> WAL-before-broadcast → 200,000-seal ring → NDJSON spill → DLQ,
> DEDUP-idempotent replay — plus an INDEPENDENT vendor-side loss proof via the
> per-symbol **Tick Sequence No** (field 15); any gap fires `TD-GAP-01` naming
> the missing range.
>
> **What is PHYSICALLY IMPOSSIBLE:** recovering intra-second trades the
> exchange/vendor conflates UPSTREAM, before they reach us. **"Never miss a
> tick" therefore means "never lose a RECEIVED message" — never "observe every
> trade".** The per-segment conflation rate and the timestamp precision are
> UNVERIFIED-LIVE; the day-0 probe MEASURES them and the measured number is
> always shown, never asserted.

### §9.5 Identity — corrections + the evidenced band (refines the §2 Identity row)

| Fact | Evidence | Effect |
|---|---|---|
| **No ISIN anywhere** in the TrueData corpus | `grep -ri isin` over the pack → 0 hits | the §2 "equities by ISIN" derivation is **not available for TrueData**; use the canonical-key scheme below |
| Symbols are subscribed **by NAME**; the wire carries a numeric Symbol ID | `truedata/03-realtime-websocket.md:44,110` | SymbolID stays a **session routing key only** (§2 already correct) |
| Numeric-id **stability across sessions/days is nowhere stated**; masters "contents change daily"; renames exist (`getSymbolNameChange`) | `truedata/00-INDEX.md:37`; `truedata/04-history-rest-api.md:50` | **Unknown** — day-0 probe; the stable-name-derived id design STANDS and is the mitigation |
| Naming: options `<ticker><YYMMDD><strike><CE\|PE>`; contract futures `<TICKER><YY><MMM>FUT` (**month only, no day**); continuous `-I/-II/-III` (synthetic); indices with spaces (`NIFTY 50`, `INDIA VIX`); BSE equities `_BSE` suffix | `truedata/06-symbols-master.md:20-43` | canonical keys below; **futures cross-feed joins are month-granular only** unless a calendar resolves the day — flagged |
| Underlying parsing | tickers contain `&` and digits (`M&M`, `L&TFH`, `3MINDIA`, `63MOONS`) | **longest-match against the known-underlying set — an alpha-prefix regex is BANNED** (it mis-splits these) |
| Symbol masters = **17 plain-text HTTPS `.txt` downloads** (`https://www.truedata.in/downloads/symbol_lists/…`), names only — no expiry/strike/lot/ISIN columns | `truedata/06-symbols-master.md:47-67` | **NEW REST/HTTP surface — NOT yet granted.** Fetching them requires a KEEP row in `no-rest-except-live-feed-2026-06-27.md` + a grant here FIRST (§5 REJECT otherwise). |

**Canonical key → id derivation (feed-neutral keys, name-derived, reconnect-proof):**

```
IDX  : "IDX|NSE|NIFTY"                       (canonicalize_index_symbol("NIFTY 50"))
EQ   : "EQ|NSE|RELIANCE" / "EQ|BSE|RELIANCE" (strip `_BSE`, exchange in key)
FUT  : "FUT|NIFTY|2019-10"                   (month granularity — vendor gives no day)
CFUT : "CFUT|NIFTY|I"                        (synthetic; EXCLUDED from cross-feed joins)
OPT  : "OPT|BANKNIFTY|2021-03-18|34500|PE"
td_security_id(k) = (fnv1a64(k) & ((1u64<<59)-1)) | (1u64<<59)   →  [2^59, 2^60)
```

A derivation **collision FAILS CLOSED** (refuse the symbol + coded error +
page) — never a silent rehash into another band (a set-dependent rehash is not
reproducible).

**Namespace bands — RANGE disjointness is mandatory (single-bit tests are UNSOUND):**

| Feed / class | Band `[lo, hi)` |
|---|---|
| Dhan + Groww numeric tokens | `[0, 2^32)` |
| GDF FNV fallback | `[2^60, 2^61)` |
| GDF token | `[2^61, 2^62)` |
| Groww index (`instruments.rs:382`, keeps low **62** bits) | `[2^62, 2^63)` |
| **TrueData (all classes)** | **`[2^59, 2^60)`** |
| TrueData reserved (unused) | `[2^58, 2^59)` |

Because the Groww derivation keeps the low **62** bits, a Groww id routinely
has bits 58–61 SET. **Therefore a "is bit N set" namespace test is UNSOUND and
BANNED**; the guard MUST be an `O(n²)` pairwise range-overlap assertion over a
const band table (`lo_a < hi_b && lo_b < hi_a`), plus `hi <= 1<<63` (positive
`i64`). **The §2 Identity row's "bit 59 token / bit 58 FNV-fallback" wording is
superseded by this single primary band + reserved band.**

### §9.6 Segment mapping (evidenced)

| TrueData master / suffix | `ExchangeSegment` |
|---|---|
| `NSE_SPOT_INDEX`, `BSE_INDEX` (`NIFTY 50`, `INDIA VIX`, `SENSEX`) | `IdxI` (0) |
| `ALL_NSE_EQ` (plain ticker) | `NseEquity` (1) |
| NSE contract futures / options / continuous futures | `NseFno` (2) |
| `ALL_BSE_EQ` / `_BSE` suffix | `BseEquity` (4) |
| BSE index options / BSE contract futures | `BseFno` (8) |
| MCX, CDS, interest-rate, BSE debt/G-sec/MF | **EXCLUDED** (§2 scope) |

### §9.7 Ratchets this correction requires (build-failing)

| # | Ratchet | Pins |
|---|---|---|
| 1 | `truedata_wire_format_guard` | no `from_le_bytes` / fixed-offset tick decode in any `truedata*` source; no `7070` constant; scope-lock carries §9 |
| 2 | `truedata_no_o1_parse_claim_guard` | source/doc scan: the TrueData parse is never described as "O(1)" |
| 3 | `truedata_namespace_bands_pairwise_disjoint` | const band table, range overlap (NOT single-bit); includes a real Groww id with bits 58–61 set that must still pass |
| 4 | `truedata_id_golden_vectors` | canonical key → exact id literal (hash/band drift fails the build) |
| 5 | `truedata_cooldown_is_five_minutes` | the ghost-session cooldown const is ~5 min, not 60 s |
| 6 | `truedata_tolerates_all_message_types` | decode dispatch handles touchline/bidask/bidaskL2/bar/greeks/marketstatus/heartbeat without panic |
| 7 | `truedata_symbol_master_fetch_is_ungranted` | no HTTP fetch of `truedata.in/downloads/symbol_lists/` exists until the REST KEEP row lands |
| 8 | `dhat_truedata_decode_zero_alloc` | zero heap alloc per frame in steady state (reusable scratch) |

### §9.8 Day-0 sandbox probe — the blocking gate (fail-closed)

No TrueData decode ships to prod until a sandbox (`wstest.truedata.in`, port
8086) capture settles, in this order:

1. Capture ONE raw welcome + ONE raw trade frame (bytes on the wire) —
   settles **LZ4-vs-plain** and **JSON array-vs-object** (the scan design
   depends on it).
2. Timestamp precision — **ms vs whole seconds**.
3. Exact raw JSON keys for `addSymbol` / `removeSymbol` (gated-PDF gap).
4. msgs/sec/symbol for 60 s at open on a liquid NIFTY future + ATM option —
   **measures the conflation rate** (§9.4).
5. Repeat (4) per segment: NSE EQ, `NIFTY 50`, `INDIA VIX`, BSE, (MCX excluded).
6. `tick_seq` per symbol for a session — gap baseline for `TD-GAP-01`.
7. SymbolID stability: same symbols on 2 consecutive days + after a mid-day
   reconnect.
8. Heartbeat gap timing + ONE deliberate reconnect — measures the honest
   at-source loss window (re-auth + re-subscribe + touchline).
9. Assigned prod port (8082 vs 8084) + entitlements confirmed in writing.

### §9.9 Operator action required BEFORE the trial (entitlements)

Ticks are enabled by default, but bar-only compositions exist and segments are
per-account. The operator must confirm with TrueData support, in writing:
**"Live data (Streaming Ticks)"** (NOT a bars-only plan) · segments **NSE EQ +
NSE Indices + NSE F&O** · symbol cap **≥ 500** · **Bid/Ask L1 ON** · the
**assigned prod port** · **sandbox access** (`wstest.truedata.in:8086`) for the
§9.8 probe.

### §9.10 What did NOT change

The §0 operator quotes; the default-OFF contract; native-Rust-only; the SSM
1-session lock design; capture-at-receipt WAL→ring→spill→DLQ; shared tables
with `feed='truedata'` + feed-in-key DEDUP; ticks-only timeframe derivation;
RAM-first absolute; the instance/reversibility contract (§3); every §5 REJECT
row. **§9 corrects OUR evidence, never the operator's scope.**

---

## §10. GROUND-TRUTH RECONCILIATION — 2026-07-24 (official PDFs supplied by the operator)

> **AUTHORITY: this section SUPERSEDES §9 wherever they conflict, and restores
> most of the ORIGINAL §1/§2 contract.** The operator supplied the previously
> email-gated official PDFs (`TrueData Market Data API Documentation v 2.6`,
> 20 Feb 2025, 27 pp; `TrueData TCP API Documentation v 2.3`, 27 Feb 2025,
> marked CONFIDENTIAL/NDA) plus the TD Postman collections. Two agents read
> both PDFs END TO END. **§9 was an HONEST correction against the in-repo web
> pack, but that pack was reconstructed WITHOUT these PDFs and therefore only
> ever saw the JSON path. §9 over-retracted.** Page numbers below are PDF
> pages; every row is quotable from the source.
>
> **Governance note (no false-OK):** §9 is NOT deleted — it is retained as the
> audit trail of what the in-repo evidence supported at the time. Where §9 and
> §10 disagree, **§10 wins**. Operator scope (§0 quotes) is untouched by both.

### §10.1 The headline: BOTH binary protocols are REAL (§9.1 rows 1-3 RETRACTED-IN-TURN)

| # | §9 claimed | The PDFs actually document | Net |
|---|---|---|---|
| 1 | 90-byte binary Trade struct "RETRACTED — unsupported" | **v2.6 p.16: full "Trade Binary" table, "Total Bytes 90", Msg Code `T`** — offsets match the ORIGINAL §2 row exactly, with types (float/int/long/double) stated | **§9 WRONG — the original §2 row was RIGHT** |
| 2 | 128-byte binary auth reply "RETRACTED" | **v2.6 p.10: full 128-byte auth table** (`A`@0, Status@1, Message 31B@2, Segment 60B@33, MaxSymbols int@93, Subscription 20B@97, Validity 10B@117) | **§9 WRONG — original RIGHT** |
| 3 | "TCP port 7070 does not exist" | **TCP v2.3 p.8: "Streaming TCP Port 7070"**, host `tcp.truedata.in`; p.13-14: **87-byte** trade packet, `Z` footer @86 | **§9 WRONG — 7070 IS real** |
| 4 | Wire is "LZ4-compressed JSON by default" | **Zero LZ4 mentions in either PDF.** GZIP is documented for `bar1min` ONLY (v2.6 p.18) | **§9 unsupported** |
| 5 | Force-logout wait "~5 minutes" | **v2.6 p.19: "Wait for 1 min after firing the URL and then try to log in"** | **§9 WRONG — original 60 s was RIGHT** |
| 6 | Bid/ask zeros appended ⇒ "length dispatch UNNECESSARY" | **19 fields with bid/ask vs 15 without (v2.6 p.16-17)** — variable length is real | **§9 WRONG — dispatch IS required** |
| 7 | Prod port 8082 | **v2.6 p.9: "Production Environment Port(s) – Real Time – 8084"**; 8086 = sandbox. 8082 appears only in the force-logout example URL | **§10: 8084 prod / 8086 sandbox** (original §2 was right) |

**The ONE genuine error in the original §2** (which §9 caught correctly, for the
wrong reason): the 90-byte / 128-byte structs are the **WEBSOCKET** binary
formats, NOT the TCP ones. TCP has its own, materially different layout (§10.3).

### §10.2 WebSocket binary — the PRIMARY implementation target (v2.6)

| Aspect | Evidenced value (v2.6) |
|---|---|
| URL | `wss://push.truedata.in:<port>?user=<U>&password=<P>` (p.9) — credentials in the query, TLS-wrapped |
| Ports | **PROD 8084**, **SANDBOX 8086**; sandbox test page `https://wstest.truedata.in/` (p.9) |
| Auth reply | JSON **or** the **128-byte binary struct** (p.10); carries `maxsymbols`, `segments`, `subscription`, `validity`. Duplicate login ⇒ `"User Already Connected"` |
| Requests (VERBATIM, **lowercase**) | `{"method":"addsymbol","symbols":[…]}` · `{"method":"removesymbol","symbols":[…]}` · `{"method":"getmarketstatus"}` · `{"method":"logout"}` (p.12-14). `"touchline"` method is **deprecated** (p.15) |
| SymbolID source | the **`addsymbol` reply** (`symbollist`) — session-scoped routing key (p.12) |
| **Trade Binary (90 B, p.16)** | `T`@0 · SymbolId i32@1 · Timestamp i32@5 · LTP f32@9 · Volume i32@13 · ATP f32@17 · TotVolume i64@21 · Open f32@29 · High f32@33 · Low f32@37 · PrevClose f32@41 · OI i64@45 · PrevOI i64@53 · Turnover f64@61 · OHL char@69 · **SeqNo i32@70** · Bid f32@74 · BidQty i32@78 · Ask f32@82 · AskQty i32@86 |
| Trade JSON (positional array) | 19 fields with bid/ask; **15 without** (p.16-17) — length dispatch required |
| **Timestamps** | **Trade = WHOLE SECONDS** (`"2020-12-16T14:02:32"` / i32 epoch-seconds). **Heartbeat = MILLISECONDS** (i64 epoch-ms). `bidask` uses a DIFFERENT format: US `M/D/YYYY h:mm:ss AM/PM` (p.17) |
| Heartbeat | every **5-6 seconds**; binary 10 B (`H`@0, status@1, epoch-ms i64@2) (p.11) |
| Other types | `marketstatus` (binary 62 B), `touchline` (17 fields, pushed at pre-open + on every subscribe), `bidask`, `bidaskL2` (BSE 5-level), `greeks` (backend-enabled), `bar1min` (GZIP, backend-enabled) (p.11-18) |
| Seq No | "helps you verify that no Ticks are lost or packets dropped" (p.16e). **Per-symbol vs global, reset, wrap: NOT STATED — Unknown** |
| Receive-buffer warning | "If not done properly this could fill up the receive buffer at your end and the send buffer at the server end leading to **packet drops**" (p.16f) — the drain-only read task is MANDATORY |
| Rate limits | stated for **historical REST only**; none for the live stream (p.19) |

### §10.3 TCP binary (v2.3) — evaluated and NOT chosen (recorded for completeness)

Host `tcp.truedata.in`, **port 7070** (p.8); ASCII `LOGIN <user> <pass>`;
**87-byte** trade packet, `Z` footer: `TR`@0 · SymbolId@2 · Timestamp@6 ·
**SeqNo@10** · LTP@14 · LastTickVol@18 · ATP@22 · **TotTradedQty (only 4 B)**@26 ·
Open@30 · High@34 · Low@38 · PrevClose@42 · OI i64@46 · PrevOI i64@54 ·
Turnover f64@62 · Bid@70 · BidQty@74 · Ask@78 · AskQty@82 · `Z`@86.
Also `BA` (27 B L1) and `L2` (139 B, BSE 5-level).

**DECISION — WebSocket binary is the chosen transport. TCP 7070 is REJECTED for
this trial** on four evidenced grounds:
1. **No TLS.** `LOGIN user pass` crosses port 7070 in **cleartext** (same hazard
   class as the GDF plain-`ws://` flag). WSS wraps the same credentials in TLS.
2. **Subscribe syntax is UNDOCUMENTED** — v2.3 p.13 says the add-symbol
   request/response "is now updated to an array" and **the example figure is
   blank**. Un-implementable without vendor confirmation. HARD BLOCKER.
3. **No heartbeat documented** for TCP (WS has 5-6 s) — we would have to invent
   an idle watchdog with no vendor contract.
4. **TotTradedQty is 4 bytes on TCP** (8 on WS) — guaranteed overflow on liquid
   symbols.
Re-opening TCP requires a fresh dated note HERE (it is not forbidden, just not
chosen). Its layout is recorded above so a future PR need not re-read the NDA PDF.

### §10.4 O(1) — RESTORED (supersedes §9.3)

§9.3 banned the word "O(1)" for the TrueData parse because it assumed a JSON
wire. **With the 90-byte WS binary frame, fixed-offset `from_le_bytes` decode
IS genuinely O(1) and zero-allocation** — the operator's "entirely RUST O(1)"
mandate is met literally on the hot path.

| Operation | Complexity | Note |
|---|---|---|
| 90-byte binary trade decode | **O(1)**, zero-alloc | 20 fixed offsets, no loop, no heap |
| Frame-type + length dispatch | **O(1)** | msg-code byte @0, length validates |
| SymbolID → stable security_id | **O(1)** avg | `papaya` lock-free map |
| Seq-gap check | **O(1)** | per-symbol last-seq compare |
| Persisted dedup / uniqueness | **O(1)** | QuestDB DEDUP UPSERT KEYS incl. `feed` |
| JSON fallback path (if binary unavailable) | **O(frame bytes)**, zero-alloc | positional scan; MUST NOT be called O(1) |
| RAM decision read | **O(log ring)** | `RwLock` + binary search — unchanged, still flagged |

**§9.3's blanket ban is REPLACED by:** the BINARY path may be described as O(1);
the JSON fallback may NOT. The `truedata_no_o1_parse_claim_guard` ratchet
(§9.7 #2) is re-scoped to the JSON path only.

### §10.5 Timestamp consequence — the dedup rule that SURVIVES (§9 was RIGHT here)

**CONFIRMED by both PDFs:** trade timestamps are **whole seconds**. Therefore
**many ticks share one `ts` for the same symbol**. A dedup key of
`(ts, security_id, exchange_segment, feed)` would SILENTLY DESTROY every tick
but the last in each second.

**BINDING:** the `ticks` DEDUP key MUST additionally carry a per-tick
disambiguator — the repo's existing `capture_seq` **and** the vendor `SeqNo`
persisted as its own column (vendor-authoritative + replay-stable). A ratchet
MUST prove that **N ticks inside one second all survive** persistence.

### §10.6 STILL UNKNOWN after both PDFs (day-0 probe settles; fail-closed)

| # | Unknown | Why it matters | Probe |
|---|---|---|---|
| 1 | **How the client SELECTS binary vs JSON** — both are documented, the switch is never stated | The entire parser choice | Ask TrueData support; capture the sandbox default |
| 2 | **Endianness** — the word appears in NEITHER PDF | Wrong guess = every price silently garbage | Decode a known symbol; LTP within ±50 % of touchline |
| 3 | Seq-No scope (per-symbol vs global), reset + wrap behaviour | False TD-GAP-01 storms, or masked real loss | Log seq per symbol for a session |
| 4 | **Granularity** — v2.6 says "sent whenever there is a trade" / "tick by tick" (p.7,8,16); the in-repo research says NSE does not redistribute TBT to ANY vendor (§9.4) | The "never miss a tick" claim | **Count msgs/sec/symbol at open**; compare vs the official 1-min bar volume for the same minute |
| 5 | SymbolID stability across sessions/days | Identity corruption on reconnect | Same symbols on 2 days + after a reconnect |
| 6 | Real-time compression (if any) | Decode path | Sandbox capture |
| 7 | Maintenance windows | Reconnect pacing | Not in either PDF; ask support |
| 8 | `subscription` string inconsistency: p.10 lists `tick+1min` combos yet also says "if you are subscribed for tick, you would not receive 1 min or 5min streaming bars and vice versa" | Entitlement expectations | Confirm in writing |

**Granularity honesty (UNCHANGED from §9.4 in substance):** the vendor PDF
asserts tick-by-tick; independent NSE-structure evidence says conflated ~1 s.
**This lock takes NO side.** What we guarantee is CAPTURE-completeness of every
RECEIVED message plus the vendor SeqNo loss-proof; "never miss a tick" means
**"never lose a received message"**, never "observe every exchange trade",
until the day-0 measurement says otherwise. The measured number is always shown.

### §10.7 Ratchet reconciliation (supersedes the §9.7 table)

| # | Ratchet | Change vs §9.7 |
|---|---|---|
| 1 | `truedata_wire_format_guard` | **INVERTED** — now REQUIRES the 90-byte const offset table + msg-code dispatch; bans a JSON-only decode as the primary path |
| 2 | `truedata_no_o1_parse_claim_guard` | **RE-SCOPED** to the JSON fallback path only (binary O(1) claim is legitimate) |
| 3 | `truedata_namespace_bands_pairwise_disjoint` | **UNCHANGED** (§9.5 identity work stands) |
| 4 | `truedata_id_golden_vectors` | **UNCHANGED** |
| 5 | `truedata_cooldown_is_one_minute` | **CORRECTED** from 5 min → **1 min** (v2.6 p.19) |
| 6 | `truedata_tolerates_all_message_types` | **UNCHANGED** + add `bidaskL2`, `bar1min`, `greeks`, deprecated-`touchline` |
| 7 | `truedata_symbol_master_fetch_is_ungranted` | **UNCHANGED** until a REST KEEP row lands |
| 8 | `dhat_truedata_decode_zero_alloc` | **UNCHANGED** (binary makes it easier) |
| 9 | **NEW** `truedata_ticks_same_second_all_survive` | §10.5 — N ticks in one second must all persist |
| 10 | **NEW** `truedata_endianness_probe_gate` | decode refuses until the probe verdict is pinned (§10.6 #2) |
| 11 | **NEW** `truedata_tcp_7070_not_used` | TCP is recorded-but-rejected (§10.3); no 7070 client may exist without a fresh dated note |

### §10.8 What did NOT change

The §0 operator quotes; default-OFF; native-Rust-only; the SSM 1-session lock;
capture-at-receipt WAL→ring→spill→DLQ; shared tables + `feed='truedata'` +
feed-in-key DEDUP; ticks-only timeframe derivation; RAM-first absolute; the
instance/reversibility contract (§3); every §5 REJECT row; and the §9.5
identity design (canonical keys, band `[2^59, 2^60)`, RANGE disjointness).
