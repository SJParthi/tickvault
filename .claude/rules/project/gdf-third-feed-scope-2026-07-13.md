# GDF Third Feed — Pluggable-Feeds Scope Extension (Operator Lock 2026-07-13)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §I > this file > `groww-second-feed-scope-2026-06-19.md` (the feed-#2 template this extends) > `websocket-connection-scope-lock.md` (EXTENDED below) > `no-rest-except-live-feed-2026-06-27.md` (GDF rows added in PR-B) > defaults.
> **Scope:** PERMANENT. Every Phase. Every PR. Every future Claude/Cowork session.
> **Operator-locked:** 2026-07-13 (verbatim quote preserved below).
> **Status:** **This file is the SCOPE AUTHORIZATION RECORD** (operator quote 2026-07-13) + the binding contract, TRIAL-FIRST. Code lands per `.claude/plans/active-plan-gdf-feed.md` (PR-A..PR-G). **Implementation PRs B–G may only start once that plan file's `**Status:**` flips to APPROVED by the operator** (design-first wall — the plan lands DRAFT in PR-A). The live GDF key is a **1-week trial** — everything ships DEFAULT-OFF and code-ready so day 0 of the trial is a config flip, not a build.
> **Ground truth:** `docs/gdf-ref/` (15-file reference pack, merged PR #1493, 2026-07-13 — incl. two operator-pasted LIVE-DOC pages). Citations below: **[R#n]** = row n of the "Reconciled master claims table" in `docs/gdf-ref/README.md` (44 adjudicated facts); **[U-n]** = row n of `docs/gdf-ref/99-UNKNOWNS.md`; **LIVE-DOC / SDK (CLIENT-LIB-SOURCE) / ARITH / Assumed / Unknown** per the README evidence-tier legend.
> **Extends (does NOT supersede):** the Dhan 2-WS lock is being retired by the parallel Dhan-removal session (its own dated rule edit); the Groww feed contract (`groww-second-feed-scope-2026-06-19.md`) is UNCHANGED. This file authorizes a SEPARATE, INDEPENDENT, **default-OFF** third market-data feed to a NEW provider (Global Data Feeds / GFDL, "GDF") under the same pluggable-feed contract.
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The verbatim operator demand (preserve exactly, do not paraphrase — typos included)

**Quote (2026-07-13):**
> "See dude now im goign to get eh access from gdf for a one week trial to fetch the every rela tiem symbols dude so we ened to precisley design the systm accoridngly dude okay? we ened to get ready with the cod eprorgram eveytrhgin else neitlry dude even inclduign db and all other factros dude because only then when they enbale the access we can diretcl yfetch the data and its repstcive detaisl for cross evrificationd dudee okay?"

(Context recorded same day: Dhan live WS is being removed in a parallel session; Groww WS
stays as feed #2; GDF is the NEW third websocket feed, trial-first, code-ready before the
key arrives, including DB readiness and cross-verification.)

---

## §1. The rule — one paragraph

Market-data feeds remain **pluggable**. A THIRD feed provider is authorized: **GDF**
(Global Data Feeds / GFDL — NSE-authorized L1 realtime vendor [R#1]), **feed
`'gdf'`, default OFF** (`feeds.gdf_enabled = false`, `#[serde(default)]`). GDF is
implemented **natively in tickvault Rust** — NOTHING is vendored from the GFDL Python/JS
SDKs or any third-party client; they are protocol REFERENCE ONLY (the Groww §32/§35
precedent). Transport is **plain `ws://` WebSocket carrying JSON** (no TLS shown anywhere
in the official corpus — [R#5]; the API key travels in a cleartext frame, flagged in §6).
The client speaks BOTH server dialects: the **long-key** per-symbol
`RealtimeResult` push (SubscribeRealtime — LIVE-DOC, `docs/gdf-ref/03-websocket-realtime-feed.md`)
AND the **short-key** `{"T":"Batch","data":[{…,"T":"RT"}]}` firehose rows
(StreamAllSymbols — LIVE-DOC, `docs/gdf-ref/10-streaming-push-api.md`), which is
authenticate-only and streams ALL symbols of the key's enabled exchanges [R#32]. GDF
reuses the existing resilience architecture **AS-IS** — capture-at-receipt
(WAL-before-broadcast, feed-parameterized `ws_frame_spill`) → ring → spill → DLQ → the
shared 21-TF aggregator (its OWN instance, `FeedStrategy::GDF`) — and writes the **SAME
shared tables** (`ticks`, `candles_1m`, `instrument_lifecycle`, `ws_event_audit`, …) with
every row tagged `feed='gdf'` and `feed` in every DEDUP key (NO `gdf_*` parallel data
tables — the 2026-06-19 operator table-model decision binds feed #3 identically). GDF
enforces **1 active session per API key** server-side (a NEW connection INVALIDATES the
previous one — [R#7]), so a **1-session-per-key SSM named lock**
(`/tickvault/<env>/instance-lock-gdf`, the GROWW-SCALE-05 / RESILIENCE-01 machinery) gates
every GDF connect, fail-closed. GDF drives NO strategy and places NO order — capture +
candles + cross-verification ONLY during the trial and until a fresh dated quote says
otherwise.

---

## §2. The pluggable-feed contract (LOCKED)

| Aspect | Locked value | Evidence |
|---|---|---|
| Feed id | `Feed::Gdf`, `as_str() = "gdf"`, `WsType::GdfFeed` | repo pattern (`crates/common/src/feed.rs` exhaustive-match design) |
| Default | **OFF** — `feeds.gdf_enabled = false`, `#[serde(default)]`; flipping the DEFAULT needs a fresh dated quote | groww-scope §2 precedent |
| Transport | plain `ws://<endpoint>:<port>` (per-account host+port issued by GDF at enablement — never hardcoded [R#4]), JSON frames, tokio-tungstenite (pinned 0.29.0), frame cap LIFTED (server sends huge single frames; the official SDK uses 2^50) | SDK (wsgfdl-py ws.py `max_size=2**50`); [R#26] |
| Auth | ONE in-band WS JSON frame `{"MessageType":"Authenticate","Password":"<API_KEY>"}` → `AuthenticateResult{Complete:true,Message:"Welcome!"}`. NOT REST [R#6]. Key = SSM read-only `Secret<String>` at `/tickvault/<env>/gdf/access-key`; endpoint at `/tickvault/<env>/gdf/endpoint`. Key NEVER logged, NEVER in URLs. | SDK verbatim; LIVE-DOC (the StreamAllSymbols page repeats the same Authenticate) |
| Session model | **1 active session per key** — a 2nd login invalidates the 1st ("Access Denied. Key already in use by other session.") [R#7]. SSM named lock `instance-lock-gdf` acquired BEFORE any GDF connect; refused → degrade (no GDF this boot), page, never fight the peer. | [R#7]; `dual-instance-lock-2026-07-04.md` §3 pattern |
| Capture modes | **Mode A (primary, trial): firehose** — StreamAllSymbols, authenticate-only, ALL enabled-exchange symbols in short-key Batch rows [LIVE-DOC, `docs/gdf-ref/10-streaming-push-api.md` §2]. **Mode B (fallback): SubscribeRealtime** per watch-entry (1 request/symbol, no batch form, long-key rows [R#12]). Config `feeds.gdf.mode = "firehose"\|"subscribe"`. Whether the firehose is a separate endpoint/entitlement from the WS API is **Unknown** (live probe day 0 — [U-1] residual). | [R#32]; [U-1] |
| Persistence split | TRACKED universe (the daily-universe ~343 SIDs + operator-named extras) → shared `ticks`/candles live, `feed='gdf'`. FULL firehose remainder → **compressed NDJSON segment files on disk ONLY** (durable, replayable) — NEVER bulk-ingested into QuestDB (sizing math in the plan Design D4: 8K–100K rows/s vs the ~5K/s ingest envelope). | plan Design D4 (firehose sizing) |
| Durable floor | capture-at-receipt: raw frame appended to the feed-parameterized WAL (`data/spill/gdf/`) BEFORE any parse/broadcast; ring→spill→DLQ unchanged; replay on boot, DEDUP-idempotent. | groww-scope §3; `ws-frame-spill-error-codes.md` |
| Aggregator | shared engine CODE, OWN instance; `FeedStrategy::GDF` (late-tick policy default **Refold** — whole-second stamps, in-process client, Dhan-class; operator may override in plan review); own `CATCHUP_SEAL_LATENESS_MARGIN_SECS_GDF`. | wave-6 BOUNDARY-01 contract |
| Identity | `security_id` = GDF `TokenNumber` (exchange numeric token from GetInstruments [R#29]) with GDF namespace bit 61; missing/non-numeric token (observed `TokenNumber:null` in a SnapshotResult sample — `docs/gdf-ref/04-websocket-snapshot-bars.md`) → FNV-1a64-derived fallback id with bit 60 + build-time fail-closed collision check; raw `InstrumentIdentifier` persisted in `instrument_lifecycle.symbol_name` (feed='gdf') for reversibility. Cross-feed joins by ISIN / canonical index symbol / (underlying, expiry) — NEVER native id (I-P1-11 + futidx-4 §2). | `docs/gdf-ref/06-instruments-and-identity.md`; `docs/gdf-ref/13-integration-proposal.md` §4; [R#29]; [U-21] |
| Segment map | `NSE→NSE_EQ(1)`, `NSE_IDX/BSE_IDX→IDX_I(0)`, `NFO→NSE_FNO(2)`, `BSE→BSE_EQ(4)`, `BFO→BSE_FNO(8)`; `CDS`/`MCX`/`BSE_DEBT` EXCLUDED (currency/commodity out of scope per the daily-universe lock). | [R#3]; `docs/gdf-ref/06-instruments-and-identity.md` §4 |
| Timestamps | WS = **UTC epoch SECONDS** (LTT/STT; no sub-second precision anywhere) [R#15, ARITH ×2]; `LAG_FLOOR_MS_GDF = 1000`. STT−LTT ≈ 1s observed [LIVE-DOC ARITH]. | [R#15]; [U-23] (per-exchange confirm) |
| Heartbeat | server-push `MessageType:"Echo"` every few seconds; client sends nothing; silence = the feed-level stall watchdog's signal. | [R#10]; [U-12] (cadence probe) |
| Reconnect | full re-auth + re-subscribe (no resume protocol) [R#11]; bounded expo ladder (5s→60s) + lock re-check; every disconnect stamps `ws_event_audit` (`WsType::GdfFeed`, feed='gdf'). | [R#11] |
| Decoder hardening | tolerate: non-JSON plain-text diagnostic frames [R#8]; 2018-era rows missing PriceChange/OIChange (all tick fields `Option`-al) [R#16]; `LTQ:0` on live rows [LIVE-DOC]; scientific-notation floats [R#17]; string booleans + case-drifting request keys [R#27]; Request/Result envelopes; `Close` = PREV-DAY close in realtime/quote family (bar close only in Snapshot/History rows) [R#14]. | [R#8]/[R#13]/[R#14]/[R#16]/[R#17]/[R#27] |
| Orders/strategy | NONE. GDF is capture + candles + cross-verify only. §28 indicators/strategies boundary untouched. | this lock |

**Run modes:** `dhan_enabled` (being retired) / `groww_enabled` / `gdf_enabled` are
independent booleans; any combination is legal; GDF OFF ⇒ byte-identical behavior to today.

---

## §3. Market-data pull inventory — KEEP rows + the GetHistory grant (mirrors `no-rest-except-live-feed-2026-06-27.md` §3/§8; that file gains these rows in PR-B)

| Endpoint / function | Transport | Class | Verdict |
|---|---|---|---|
| `Authenticate` (in-band WS frame) | WS | live-feed AUTH | **KEEP** — not REST at all; the WS session cannot exist without it |
| `GetLimitation` + `GetServerInfo` + `GetExchanges` (in-band, at connect) | WS | live-feed session metadata — the per-key entitlement oracle (functions on/off, per-exchange symbol caps, DataDelay, history caps) [R#23] | **KEEP** — day-0 probe + every boot; logged verbatim (sans key) |
| `GetInstruments` (per enabled exchange, daily pre-market) | **WS in-band** (decision: WS over REST — same authenticated socket + same credential path; the REST twin puts `accessKey` in a GET query string [R#35], i.e. a token-in-URL surface the api-introduction rule bans; WS also avoids adding ANY new REST caller) | instrument-master (static ref) — the MAP source (TokenNumber, ISIN, TradeSymbol, QuotationLot, expiry, strike [R#29]) | **KEEP** — the GDF analog of the Dhan Detailed CSV / Groww master CSV rows |
| `GetHistory` (MINUTE/1) — **scheduled-pull grant, §8-style** | WS | market-data pull — post-close (15:50 IST) 1-minute history for the TRACKED comparison set ONLY (indices + the §36 futures + operator-named symbols; bounded ≤ ~25 symbols/day), for the daily GDF-official-vs-our-GDF-candles AND GDF-vs-Groww cross-verification the operator's quote demands ("for cross evrificationd"). Writes ONLY the cross-verify audit table — NEVER `ticks`/`candles_*`/`historical_candles` (live-feed purity rule). Depth per key via GetLimitation (MaxIntraday observed 44 trading days [R#22] — Assumed plan-dependent). | **KEEP (granted 2026-07-13 by §0)** |
| `SubscribeRealtime` / `StreamAllSymbols` | WS | THE live feed | **KEEP** (they are the feed) |
| GDF REST API (`http://…/?accessKey=`) — ALL functions | REST | sessionless GET with the key in the URL [R#35] | **FORBIDDEN** — no GDF REST caller may exist without a fresh dated quote here first |
| `GetHistoryAfterMarket`, Greeks family, OptionChain family, GainersLosers, VolumeShockers, Delayed API, `GetExchangeSnapshot` | WS | market-data pulls beyond the grant | **FORBIDDEN** until a fresh dated quote (GetExchangeSnapshot noted as the whole-exchange fallback if StreamAllSymbols is not entitled [R#33] — flag to operator, do not build) |

---

## §4. What is UNCHANGED (still locked)

- The Groww feed contract, tables, sidecar/native client, token-minter lock — untouched.
- SAME shared tables + `feed` column; NO `gdf_*` parallel data tables; `feed` in every
  persisted DEDUP key (`data-integrity.md` feed-in-key EVERYWHERE).
- Composite `(security_id, exchange_segment)` uniqueness per I-P1-11 in every collection.
- The resilience chain (WAL/ring/spill/DLQ/aggregator) is REUSED, never redesigned.
- Indicators/strategies boundary (daily-universe §28); dry_run stays true; no order path.
- `POST /api/feeds/gdf` bearer-gated unconditionally; `GET /api/feeds*` public read.
- Design-first wall, serial-PR protocol, All-Green merge gate, 15+7 guarantee matrices.

---

## §5. What a PR that violates this lock looks like (REJECT)

- Starts ANY implementation PR (B–G) while `.claude/plans/active-plan-gdf-feed.md` `**Status:**` is still DRAFT (the design-first wall; PR-A lands the plan DRAFT — only the operator flips it to APPROVED).
- Ships `feeds.gdf_enabled = true` (or `gdf.mode` auto-enabling) as the DEFAULT without a fresh dated quote.
- Creates any `gdf_*` parallel DATA table, or writes a shared-table row without `feed='gdf'`, or omits `feed` from a DEDUP key.
- Vendors/imports ANY GFDL SDK code (Python/JS/.NET) or adds a Python sidecar for GDF (native Rust only — the Groww §32 carve-out does NOT extend to GDF).
- Bulk-ingests the FULL firehose into QuestDB (the sizing math forbids it; tracked-universe-only live ingest).
- Adds any GDF REST caller, or any WS market-data function beyond §3 (Greeks/OptionChain/ExchangeSnapshot/AfterMarket/Delayed) without a dated §3 edit FIRST.
- Routes GetHistory output into `ticks` / `candles_*` / `historical_candles` (live-feed purity).
- Opens a GDF connection without holding the `instance-lock-gdf` named lock, or fights the "key already in use" reject with a reconnect storm.
- Wires GDF into any strategy/order/risk path.
- Gives GDF a weaker durable floor than Groww/Dhan (skips capture-at-receipt / WAL-before-broadcast).
- Logs the API key, embeds it in a URL, or stores it outside SSM + in-memory `Secret<String>`.
- Claims TBT, sub-second timestamps, or depth for GDF anywhere (see §6).

Any such PR MUST be rejected in review even if the operator approves verbally — the
operator must update this rule file FIRST with a dated quote.

---

## §6. Honest envelope (mandatory per `operator-charter-forever.md` §F / zero-loss charter §1)

> "100% inside the tested envelope, with ratcheted regression coverage: every GDF frame we
> RECEIVE is durably captured at receipt (WAL-before-broadcast → ring → spill → DLQ,
> DEDUP-idempotent replay) — **CAPTURE-complete**. NOT claimed — **UPSTREAM is a 1-second
> CONFLATED L1 feed**: GDF's documented cadence is ~1 update/sec/symbol of a full-image
> best-bid/ask L1 snapshot [R#12]; intra-second trades are conflated by the
> vendor/exchange broadcast BEFORE they reach us (`docs/gdf-ref/12-data-quality-assessment.md`);
> timestamps are whole UTC epoch-seconds with NO sub-second precision [R#15]. We NEVER
> claim tick-by-tick, never claim depth (L1 only — [R#31]), never claim millisecond stamps
> (REFUTED — the `docs/gdf-ref/README.md` refuted-claims table). The transport is plain
> `ws://` — the API key crosses the network in cleartext ([R#5]; wss availability is a
> sales question, [U-2]); accepted for the 1-week trial, flagged for the operator before
> any long-term contract. Firehose coverage equals the key's enabled exchanges + symbol
> caps (GetLimitation is the only authority — [R#22]/[R#23]); trial entitlements are
> Unknown until the day-0 probe [U-9]. Minute-bar open-vs-close stamping [U-6], batch
> cadence under burst [U-1], Echo cadence [U-12], and the streaming endpoint identity
> [U-1] are Assumed/Unknown pending the live probes listed in the plan's Trial-Day-0
> runbook."

---

## §7. Auto-driver / Insta-reel explanation

> Sir, the juice shop is changing suppliers. Supplier Dhan's price board is being taken
> down. Supplier Groww's board stays. And now a THIRD supplier — GDF — offers a one-week
> FREE TASTE: one phone line that, the moment you say the password, starts reading out the
> price of EVERY fruit in the whole market, once per second, in shorthand. We are building
> the listening desk NOW — notebook, tape recorder, filing cabinet, everything — so the
> second GDF switches the line on, we just pick up the phone. Two honest warnings on the
> wall: (1) GDF reads prices once a second — if a fruit changed price three times inside
> one second, we hear only the last one; that is how their line works, we never pretend
> otherwise. (2) The line is an open party line (no encryption) — fine for a taste week,
> but we ask about a private line before signing anything. And GDF allows only ONE
> listener per password — so a lock on our door makes sure two of our own boys never
> fight over the same phone.

---

## §8. Trigger (auto-loaded paths)

Always loaded. Activates on any session that:
- Edits `crates/common/src/feed.rs`, `crates/common/src/config.rs` (`FeedsConfig`/`GdfConfig`), `crates/common/src/ws_event_types.rs`
- Adds/edits any file under `crates/*/src/**/gdf*` or containing `Feed::Gdf`, `gdf_enabled`, `GdfFeed`, `StreamAllSymbols`, `SubscribeRealtime`, `AuthenticateResult`, `RealtimeResult`, `"T":"Batch"`, `instance-lock-gdf`, `/tickvault/<env>/gdf/`
- Edits `config/base.toml` `[feeds]` / `[feeds.gdf]` sections
- Edits any file under `docs/gdf-ref/`
- Adds any new `ws://` or `wss://` URL constant
- Edits `.claude/plans/active-plan-gdf-feed.md`

---

## §9. 2026-07-21 Amendment — SubscribeRealtime 99-symbol trial scope (operator directives)

### §9.0 The verbatim operator demands (preserve exactly, do not paraphrase — typos included)

**Quote 1 (2026-07-21, the 100-symbol SubscribeRealtime directive):**
> "Dude as of now i planned to just implement the gdfl subscrieb real time 100 symbols dude what do you say and think bro so that we can have evry secodn lvie feed data right firts chekc this newer requrimeent and tell me dud ehow doe shtiw ork bro okay?"

**Quote 2 (2026-07-21, the exact instrument set + greeks ownership):**
> "Dude as of now i planned to use only nifty 50 spot and the current expiry 98 options coevered entire call and put right alone dude okay? see we have greekks entirley in our brutex where we cna siue that dude okay?"

**Quote 3 (2026-07-21, option-buying slippage focus + the frame family):**
> "see especially wheneve we focus on otpion buyign to redcuce the amssive slippage how shdou lw e use this dude see and even our tiemframe will be liek thsi ddue whcu is 1 seocnd till 15 seocnd sequantially one by oen and 30 second and 1m, 3m, 5m, 15m alone dude okay?"

**Quote 4 (2026-07-21, daily instrument re-resolution):**
> "Eeven everyday it hsodu leven cosndier the isntruremnts and base don it onl yit shdou lsubscirbe etc etec do everythgin rigth dud ema i irgith bro tel lem dude okay?"

**Quote 5 (2026-07-21, architecture ownership delegated to Claude):**
> "i eman i simply said wal or ring butffer but id otn have an idea abotu this fuckig archtiecture bro okay? So it is your fuckign repsosnsibility to check thsi fuckign ultimate new design new arhcietcutre everthyghin rbno oklay?"

### §9.1 The recorded effects (dated contract changes)

| # | Effect | Detail |
|---|---|---|
| (a) | **Capture mode PRIMARY flips: Mode A firehose → Mode B SubscribeRealtime** | Exactly **99 symbols** = the NIFTY 50 spot index + the current-expiry strike window of **49 CE + 49 PE** NIFTY option contracts ("100 symbols" in Quote 1 = the operator's round number; Quote 2 fixes the exact set at 1 + 98 = 99). The §2 "Capture modes" row's Mode A/Mode B roles INVERT for this trial: SubscribeRealtime per-symbol is the primary lane; the firehose remains a documented, config-selectable fallback (`feeds.gdf.mode`), NOT the trial default. The D4 firehose sizing + the firehose-ingest ban stay valid as-is for any future firehose use. |
| (b) | **Daily pre-market instrument re-resolution** | Per Quote 4: every trading morning the 99-symbol set is RE-DERIVED from the in-band `GetInstruments` master (the §3 KEEP row) — current expiry re-selected (nearest ≥ today, never-roll), the 49+49 strike ladder re-centered — NEVER a stale hardcoded list. Fail-closed on <99 resolved with named gaps (design doc §2). |
| (c) | **Greeks are computed ENTIRELY in BruteX** | Per Quote 2: tickvault stores NO greeks columns/tables for GDF. The BruteX consumption seam is the §37 read-only S3 artifact class (`groww-second-feed-scope-2026-06-19.md` §37 + `brutex-readonly-lock-2026-07-18.md` — bruteX repo read-only, operator-side relay). Any tickvault-side greeks computation/storage = REJECT without a fresh dated quote. |
| (d) | **The GDF-derived OHLCV frame family** | Per Quote 3: **1s, 2s, 3s, … 15s (sequential one-by-one) + 30s + 1m, 3m, 5m, 15m** — built from the SubscribeRealtime stream for the 99 symbols. **+1d broker-pulled next pre-market** — FLAGGED: this 1d row is a COORDINATOR ASSUMPTION pending operator veto, NOT quoted; the operator's quote names only 1s–15s + 30s + 1m/3m/5m/15m. Purpose per Quote 3: option-buying slippage reduction analysis (sub-minute fill modeling). No strategy wiring — the §28 indicators/strategies boundary stands. |
| (e) | **Architecture ownership delegated to Claude** | Per Quote 5: the WAL/ring-buffer/capture architecture decisions are Claude's responsibility to design and verify (within the locked reuse-AS-IS contract below). The design record is `docs/architecture/gdf-1s-subscribe-design.md`. |

### §9.2 What is UNCHANGED by this amendment (the 2026-07-13 lock stands)

- **Default OFF** — `feeds.gdf_enabled = false`, serde default OFF; flipping the default still needs a fresh dated quote.
- **WAL/ring/spill/DLQ/aggregator reuse AS-IS** — capture-at-receipt, WAL-before-broadcast, DEDUP-idempotent replay (§2 durable floor).
- **Shared tables, `feed='gdf'`, feed-in-key DEDUP** — NO `gdf_*` parallel data tables.
- **No GDF REST** (§3 FORBIDDEN row), **no SDK vendoring** (native Rust only), **1-session-per-key `instance-lock-gdf`** fail-closed gate.
- **No strategy / no orders** — capture + candles + cross-verification only; §28 boundary untouched.
- The §5 REJECT list, §6 honest envelope (1-second conflated L1, whole-second stamps, plain `ws://` cleartext-key trial flag) all stand; where §2's "Mode A (primary, trial)" wording conflicts with §9.1(a), this amendment wins (house convention: dated amendment > earlier table row).

### §9.3 Trigger

Covered by the §8 trigger list (this file always loads; `SubscribeRealtime` / `docs/gdf-ref/` / the plan file paths already activate it). The design doc `docs/architecture/gdf-1s-subscribe-design.md` joins the reinforcement set.
