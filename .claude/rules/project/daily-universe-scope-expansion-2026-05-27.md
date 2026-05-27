# Daily Universe Scope Expansion — Operator Lock 2026-05-27

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §I > this file > `websocket-connection-scope-lock.md` (SUPERSEDED below) > `aws-budget.md` (SUPERSEDED below) > defaults.
> **Scope:** PERMANENT. Every Phase. Every PR. Every future Claude/Cowork session.
> **Operator-locked:** 2026-05-27 (verbatim quotes preserved below).
> **Status:** Sub-PR #0 of the 14-PR sequence drafted in the 2026-05-27 planning chat.
> **Supersedes:** the 2026-05-18 t4g.medium lock + 2026-05-15 `Indices4Only` scope lock + the 4-SID `LOCKED_UNIVERSE`. Prior sections retained as historical audit trail in the 4 cross-referenced files.
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The verbatim operator demands (preserve exactly, do not paraphrase)

**Quote 1 (2026-05-27, universe expansion):**
> "now in our logic once again we need to pull the entire instruments as it is dude … before 9 am around 8.45 am itself the app should be started and then it should pull the instruments entirely every new instruments file entirely and it should find the entire unique fno instruments and along with that we need all nse indexes also separately"

**Quote 2 (2026-05-27, BSE SENSEX inclusion + Quote mode + no API fallback + AWS start time):**
> "all nse indices along with one sensex bse index also needed dude entirely for all of them it should be quote mode dude always okay? no need of any api pull in case of any failures okay? see as of now AWS should be started only at 8.45 am if everything can be handled and tackled precisely then go ahead and approved"

**Quote 3 (2026-05-27, instance upgrade + tick loss + RAM + 21 TFs + CloudWatch-only):**
> "ok dude go ahead with t4g large dude previously i believe we already launched t4g medium dude that one i believe then now our script auto script should update it to large right dude? see irrespective of any situations not even a single tick should be missed meanwhile everything should be in memory RAM right bro even along with calculating entire 21 timeframes i believe bro only cloudwatch alone as well bro okay?"

**Quote 4 (2026-05-27, infinite retry + lifecycle preservation):**
> "see irrespective of any situations it should never ever fail i mean until or unless without the proper fetch it should retry right that too covering all kinds of entire extreme worst case scenarios errors exceptions situations bugs … for future or options it should be just marked as expired and active alone only right dude instead of deleting it right dude"

**Approvals:**
- 2026-05-27: Approved Sub-PR plan items A–D (infinite retry policy, single `instrument_lifecycle` table, separate `instrument_lifecycle_audit` table, plan growing to 14 sub-PRs)
- 2026-05-27: Approved options X–Z (EventBridge cron at 08:30 IST, `lifecycle_state_locked` column for operator overrides, `--dry-run-universe` CLI flag for first prod validation)

---

## §1. The rule — one paragraph

This product, starting from the date of this lock, opens exactly TWO WebSocket connections to Dhan FOREVER (unchanged from prior lock): one main-feed + one order-update. The main-feed connection subscribes to a **daily universe** of approximately **250 instrument SecurityIds** — **all NSE indices in IDX_I segment + 1 BSE SENSEX in IDX_I segment + the unique F&O underlying spot instruments (NSE_EQ)** — pulled fresh every trading morning at 08:45 IST from Dhan's static Detailed CSV. Every subscription is in **Quote mode** (Feed Request Code 17, 50-byte response packets carrying day OHLC). The previous 4-SID `LOCKED_UNIVERSE` const + `SubscriptionScope::Indices4Only` single-variant enum + the 4-IDX_I-only ratchet are RETIRED. The new compile-time enum variant is `SubscriptionScope::DailyUniverse`. The host instance upgrades from t4g.medium 4 GiB → **t4g.large 8 GiB** to hold today + yesterday sealed bars across all 21 timeframes for ~250 SIDs in RAM.

---

## §2. The complete allowed set (POST-2026-05-27)

| WebSocket | Count | Endpoint | Allowed instruments | Mode |
|---|---|---|---|---|
| **Main feed** | **1** | `wss://api-feed.dhan.co?version=2&token=<JWT>&clientId=<ID>&authType=2` | Daily-fetched universe (~250 SIDs): all `IDX_I` rows where `EXCH_ID IN (NSE, BSE)` AND `INSTRUMENT == INDEX` (~30) + every unique `UNDERLYING_SECURITY_ID` referenced by `FUTIDX/OPTIDX/FUTSTK/OPTSTK` rows, resolved to its NSE_EQ row (~218) | **Quote (request code 17)** — 50-byte packets, gives day OHLC at fixed byte offsets |
| **Order update** | **1** | `wss://api-order-update.dhan.co` | Receives order events for orders WE place; filter `Source=P` | JSON, MsgCode 42 auth |

**Total live WebSocket connections to Dhan: 2** (UNCHANGED from prior lock).

**Universe size envelope (mechanical bound):** `MAX_DAILY_UNIVERSE_SIZE = 400`. Boot HALTS if computed universe is outside `[100, 400]`. Fits comfortably on 1 main-feed connection (Dhan cap = 5,000 SIDs/conn).

**Subscription dispatch:** 250 SIDs sent in 3 JSON batches (Dhan cap = 100 SIDs/message), sequential with `SubscribeRxGuard` (PR #337) preserving subscription state across reconnects.

---

## §3. Source of the daily universe — Dhan Detailed CSV (LOCKED)

| Field | Locked value |
|---|---|
| URL | `https://images.dhan.co/api-data/api-scrip-master-detailed.csv` |
| Authentication | NONE (public static file) |
| HTTP method | GET |
| Expected size | 5–15 MB |
| Trigger time | 08:45 IST daily (after EC2 cold-boot + Step 1-6 auth completes) |
| Refresh policy | Daily — yesterday's local copy overwritten |
| Verification | SHA-256 + row count + per-row schema validation |
| Reject threshold | If >0.1% of rows fail mandatory-field validation → reject + retry. Mandatory fields: `SECURITY_ID`, `EXCH_ID`, `SEGMENT`, `INSTRUMENT`, `UNDERLYING_SECURITY_ID` (for derivatives), `SYMBOL_NAME` |
| Cross-validation | Every `UNDERLYING_SECURITY_ID` cited by FUTSTK/OPTSTK rows MUST exist as an NSE_EQ row in the same CSV — dangling references reject the CSV |

**No fallback to a different data source.** Per operator Quote 2: "no need of any api pull in case of any failures". Specifically forbidden:
- Per-segment REST `/v2/instrument/{exchangeSegment}` fallback — banned
- REST `/v2/marketfeed/ltp` as a price source — banned
- S3 cached snapshot of yesterday's CSV — banned
- Continuing boot with stale `instrument_lifecycle` data — banned

---

## §4. Infinite retry policy (LOCKED per operator Quote 4)

| Attempt | Backoff before next | Telegram severity | Action |
|---|---|---|---|
| 1–3 | 10s / 20s / 40s | None | Silent retry |
| 4–10 | min(2^N × 10s, 300s) | `Severity::Info` every attempt | Counter `tv_instrument_fetch_retries_total` increments |
| 11–20 | capped 300s | `Severity::High` every 5 attempts | Operator notified |
| 21+ | capped 300s | `Severity::Critical` every 5 attempts | Operator paged on every channel |
| ∞ | Never give up | — | Boot remains BLOCKED. No subscription dispatch. No fallback. |

**Boot stays BLOCKED until a fresh, validated CSV is in hand and the daily universe is built.** The system NEVER silently proceeds with stale or partial data. At 09:00 IST with no fresh CSV: market opens, no ticks flow, operator receives Critical and decides whether to manually intervene (e.g. check Dhan status, fix DNS, fix egress). The honest outcome is documented — never camouflaged.

**No-give-up rationale:** the operator's literal demand is "never ever fail … without the proper fetch it should retry". Fail-closed means we refuse to proceed with the wrong universe; the retry never terminates because there is no acceptable alternate path.

---

## §5. The `instrument_lifecycle` table — single source of truth, NEVER DELETE

Per operator Quote 4: "for future or options it should be just marked as expired and active alone only right dude instead of deleting it".

| Column | Type | Purpose |
|---|---|---|
| `ts` | TIMESTAMP | Designated timestamp (last update) |
| `security_id` | LONG | Dhan SecurityId |
| `exchange_segment` | SYMBOL | `IDX_I` / `NSE_EQ` / `NSE_FNO` / `BSE_EQ` / `BSE_FNO` |
| `exchange_id` | SYMBOL | `NSE` / `BSE` / `MCX` |
| `instrument_type` | SYMBOL | `INDEX` / `EQUITY` / `FUTSTK` / `OPTSTK` / `FUTIDX` / `OPTIDX` / etc. |
| `symbol_name` | SYMBOL | Tradable symbol |
| `display_name` | STRING | Human-readable |
| `underlying_security_id` | LONG | For derivatives (null for spot) |
| `underlying_symbol` | SYMBOL | For derivatives |
| `lot_size` | INT | Trading lot |
| `tick_size` | DOUBLE | Min price increment |
| `expiry_date` | TIMESTAMP | For derivatives (null for spot/index) |
| `strike_price` | DOUBLE | For options |
| `option_type` | SYMBOL | `CE` / `PE` / null |
| `lifecycle_state` | SYMBOL | `active` / `expired_from_fno` / `expired_contract` / `expired_index` / `delisted` |
| `lifecycle_state_locked` | BOOLEAN | Per option Y approval — operator manual override; orchestrator skips locked rows when flipping states |
| `first_seen_date` | TIMESTAMP | First time this SID appeared in any CSV |
| `last_seen_date` | TIMESTAMP | Last CSV that contained this SID |
| `last_active_date` | TIMESTAMP | Last date this was `active` |
| `expired_date` | TIMESTAMP | When state flipped to any `expired_*` |
| `prev_symbol` | SYMBOL | Previous symbol (if renamed via merger, e.g. HDFC → HDFCBANK) |
| `source_csv_sha256` | SYMBOL | Provenance |
| **DEDUP UPSERT KEYS** | `(security_id, exchange_segment)` per I-P1-11 composite-uniqueness rule | One row per instrument EVER observed; never deleted |

**Daily orchestrator algorithm (idempotent):**
1. UPSERT every row from today's validated CSV.
2. Scan rows where `last_seen_date < today AND lifecycle_state == active AND NOT lifecycle_state_locked` — flip `lifecycle_state` to the appropriate `expired_*` value, set `expired_date = today`.
3. Emit a `instrument_lifecycle_audit` forensic row per state transition (see §6).

---

## §6. The `instrument_lifecycle_audit` table — forensic chain for state transitions

| Column | Purpose |
|---|---|
| `ts` | When the transition was logged |
| `trading_date_ist` | The trading day on which this happened (IST) |
| `security_id` + `exchange_segment` | Which instrument (composite key per I-P1-11) |
| `from_state` SYMBOL | Previous `lifecycle_state` |
| `to_state` SYMBOL | New `lifecycle_state` |
| `transition_kind` SYMBOL | `appeared` / `updated` / `expired` / `reactivated` / `delisted_manual` / `locked` |
| `field_deltas` STRING | JSON of changed field names + before/after values (when `transition_kind = updated`) |
| `source_csv_sha256` SYMBOL | Provenance of the triggering CSV |
| `operator_note` STRING | Free-form note (populated only by manual overrides) |
| **DEDUP KEYS** | `(trading_date_ist, security_id, exchange_segment, transition_kind)` |

SEBI retention: 5 years (matches the `order_audit` table standard).

---

## §7. Instance lock — t4g.large (LOCKED 2026-05-27, supersedes 2026-05-18 t4g.medium)

| Spec | Value |
|---|---|
| Instance | **t4g.large** — ARM Graviton, **2 vCPU, 8 GiB RAM** |
| Region | ap-south-1 (Mumbai) |
| Tenancy | Default (Shared) |
| Pricing | On-demand $0.0448/hr — no Reserved / Savings Plan / Spot |
| Schedule | **08:30–17:00 IST every day Mon–Sun** (was 08:00–17:00; new cron `cron(0 3 * * ? *)` for 08:30 IST start) |
| EBS | gp3 10 GB (unchanged) |
| EIP | 1 (24/7, Dhan static-IP mandate — unchanged) |
| Network | ENA enabled by default |

### Cost bill (LOCKED ~₹1,571/mo)

| Line | Calc | Monthly ₹ |
|---|---|---|
| EC2 t4g.large | $0.0448/hr × 8.5hr × 30 × ₹85 | ₹971 |
| EIP (24/7) | $0.005/hr × 720h × ₹85 | ₹306 |
| EBS gp3 10 GB | $0.0912 × 10 × ₹85 | ₹78 |
| S3 cold | tiny dataset | ₹15 |
| CloudWatch | free tier (10/10/5GB) | ₹0 |
| SNS SMS | 100 msgs × $0.00278 × ₹85 | ₹24 |
| SNS Email / Lambda | free tier | ₹0 |
| Data transfer | ~14 GB outbound (more SIDs) | ₹120 |
| **TOTAL** | | **~₹1,514/mo** |

**Honest envelope:** ~₹1,514/mo is **+₹492 over the previous ₹1,022/mo t4g.medium lock** and **+₹514 over the original <₹1,000 target**. Operator approved 2026-05-27.

> **Note on instance schedule:** the original prior-lock schedule was 08:00–17:00 IST (9 hours). The new schedule shifts the start to 08:30 IST per option X approval (gives 15-min cushion for cold-boot + Step 1–6 + CSV retry budget so the app is ready before 09:00 market open). End-of-day stays 17:00 IST. Total runtime ≈ 8.5 hours; if operator prefers full 9 hours (08:30–17:30 IST), update this section.

### Mechanical Rules (replaces aws-budget.md mechanical rules 1+6)

1. **Instance type is t4g.large. PERIOD.** Going larger (t4g.xlarge, c7g, m7g) requires:
   - Operator explicit approval with dated quote
   - Update to this file
   - Update to `aws-indices-only-locked-architecture.md` §5
   - Update to `aws-budget.md` (existing file marked SUPERSEDED)
   - Ratchet test `crates/storage/tests/instance_type_lock_guard.rs` updated to pin the new type

2. **Host memory budget for t4g.large (8 GiB total) — POST CloudWatch-only migration, 250 SIDs across 21 TFs:**
   - QuestDB process: ~2.5 GB (more write pressure — ~5000 ticks/sec sustained, ~2500 audit rows/min)
   - Tickvault app: registry + 21-TF aggregator + indicator state + today/yesterday RAM-resident sealed bars (≈3.2 MB × 250 SIDs) ≈ **~800 MB**
   - App: rescue ring (100K tick cap, fixed): 10 MB
   - App: QuestDB ILP write buffer: 25 MB
   - App: 15+ audit-table buffers: 30 MB
   - Tracing / errors.jsonl rotation buffer: 100 MB
   - OS + FS cache + kernel TCP buffers: ~600 MB
   - **Total used: ~4.1 GB**
   - **Headroom: ~3.9 GB** — well above the 1 GB Linux kswapd floor

3. **EBS stays at 10 GB.** Partition manager auto-archives to S3 at 90d. Estimated ~50 MB/day across all tables for 250 SIDs × 21 TFs ≈ 200 days of hot data fits in 10 GB.

4. **No paid AWS services** (RDS, ElastiCache, NAT Gateway, ALB) without budget review.

5. **CloudWatch is the sole observability layer** — within free tier (10 metrics + 10 alarms + 5 GB logs).

6. **RAM-first hot path (mandatory, unchanged):** every indicator + strategy + risk decision reads from RAM. QuestDB is persistence + audit + cold-path boot rehydration only. Banned-pattern scanner enforces.

7. **One-time instance upgrade script:** `scripts/aws-upgrade-instance.sh` performs the t4g.medium → t4g.large flip via `aws ec2 stop-instances` → `aws ec2 modify-instance-attribute` → `aws ec2 start-instances`. EIP + EBS preserved. Downtime ~3 minutes, scheduled Sunday 22:00 IST (off-market).

---

## §8. Subscription mode — Quote for every SID (LOCKED per operator Quote 2)

Every SID in the daily universe — indices AND F&O underlyings — subscribes in **Quote mode**:

- Feed Request Code: `17` (Subscribe — Quote Packet)
- Response Code: `4` (Quote Packet)
- Packet size: **50 bytes**
- Carries: LTP, LTQ, LTT (IST epoch secs), ATP, Volume, Total Sell Qty, Total Buy Qty, **day_open** (bytes 34-37), **previous-day close** (bytes 38-41), day_high (bytes 42-45), day_low (bytes 46-49)

**Why Quote and not Ticker:** the Quote packet delivers session day OHLC at fixed byte offsets, removing the need for app-side `DayOhlcTracker` state for the universe-wide subscription. For derivatives prev-close routing, Quote mode is correct for NSE_EQ underlyings per `live-market-feed.md` rule 10. (Indices in `IDX_I` segment also receive Quote-mode packets per operator's 2026-05-20 directive — `DayOhlcTracker` falls back to first-tick LTP auto-arm if Dhan ignores Quote for IDX_I.)

**Bandwidth envelope:** 250 SIDs × 50 B/packet × ~20 ticks/sec sustained ≈ 250 KB/sec. Within AWS egress + Dhan WebSocket limits.

---

## §9. Z+ 7-layer defense for the instrument-master fetch

Per `.claude/rules/project/z-plus-defense-doctrine.md`:

| Layer | Mechanism | What it catches |
|---|---|---|
| **L1 DETECT** | HTTPS GET on `images.dhan.co/api-data/api-scrip-master-detailed.csv`; check 200 OK + content-length > 1 MB | Network failure, Dhan 5xx, empty response |
| **L2 VERIFY** | Parse CSV header row; SHA-256 of payload; row count; mandatory-column presence; TLS cert validation | Schema drift, partial download, corruption, DNS poisoning |
| **L3 RECONCILE** | Compare row count + SHA-256 vs yesterday's `instrument_fetch_audit`; flag if `total_rows < 0.5× yesterday OR > 2× yesterday` | Silent Dhan-side bulk changes, stale-CSV serving |
| **L4 PREVENT** | Validate parsed universe size in `[100, 400]`; HALT if outside. Verify every `UNDERLYING_SECURITY_ID` from FUTSTK/OPTSTK rows resolves to an NSE_EQ row in the same CSV (no dangling references). | Misparsed CSV; runaway subscription cost |
| **L5 AUDIT** | `instrument_fetch_audit` row per attempt (success or failure) + `instrument_lifecycle_audit` row per state transition + `tv_universe_size{kind=...}` CloudWatch metric + tracing span | Continuous observability + forensic chain |
| **L6 RECOVER** | Infinite retry (per §4). NEVER fallback to a different data source. NEVER proceed with stale data. | Operator-locked: no silent degradation |
| **L7 COOLDOWN** | Retry backoff capped at 300s between attempts; never burst Dhan with >5 GETs/min | Self-induced rate limit |

---

## §10. Boot sequence integration

The new `instrument_lifecycle` orchestrator slots between existing Step 6 (auth) and the WebSocket pool spawn:

```
08:30 IST  EventBridge cron fires; EC2 t4g.large boots (~60s cold)
08:31      Docker compose up (QuestDB + tickvault-app)
08:31:30   App Step 1-5: CryptoProvider → Config → Observability → Logging → Notification
08:32      Step 6: Dhan auth (TOTP → JWT) — Valkey dual-instance lock acquired
08:32:30   Step 6a: Dhan static IP boot gate (GET /v2/ip/getIP)
08:33      Step 6b: QuestDB DDL — includes new `instrument_lifecycle` + `instrument_lifecycle_audit` tables
08:33:30   ─── NEW: Step 6c — Daily universe orchestrator ─────────────────────
              1. Read yesterday's active set from `instrument_lifecycle` (cold-path bootstrap)
              2. GET Dhan Detailed CSV with L1-L7 defense layers (§9)
              3. Validate + parse + SHA-256
              4. Extract indices (filter §2) + unique F&O underlyings (group by UNDERLYING_SECURITY_ID)
              5. Compute delta vs yesterday — emit added / expired / renamed transitions
              6. UPSERT `instrument_lifecycle` + INSERT `instrument_lifecycle_audit`
              7. INSERT `instrument_fetch_audit` outcome row
              8. Build `Arc<DailyUniverse>` for the WS subscription dispatcher
              9. Bust rkyv binary cache; rebuild for tomorrow's warm boot
              [Infinite retry on any L1-L4 failure per §4]
08:34      Step 7: Spawn 1 main-feed WebSocket; subscribe Quote mode in 3 batches
08:34:30   Step 8: Spawn order-update WebSocket
08:35      Phase-1 boot complete; listening for ticks (10 min before market open)
08:45      ── soft deadline — if not subscribed by here, fire Severity::High Telegram
08:55      ── hard deadline — if not subscribed by here, fire Severity::Critical Telegram
09:00      Market opens — universe is live
```

**Boot-time-of-day guard:** if `now > 08:55 IST` at orchestrator start, refuse to begin a fresh-fetch cycle without explicit operator override flag (`--allow-late-boot`). Avoids mid-market universe rebuild attempts.

**Day 1 bootstrap:** if `instrument_lifecycle` table is empty (very first run), every CSV row gets `first_seen_date = today`, `lifecycle_state = active`, NO delta-expiry pass. Detected by SELECT COUNT(*) before the orchestrator's flip pass.

**`--dry-run-universe` CLI flag (per option Z approval):** fetches + validates + computes universe + writes audit row but does NOT subscribe. Use on first production boot to verify pipeline end-to-end without committing to live ticks.

---

## §11. Mechanical guards (to be wired by Sub-PR #1 onward)

| Guard | What it enforces | Test file (Sub-PR that adds it) |
|---|---|---|
| `SubscriptionScope::DailyUniverse` enum variant | Compile-time scope contract; retires `Indices4Only` | `crates/common/src/config.rs` (Sub-PR #1) |
| `effective_main_feed_pool_size(_, _) → 1` | Main feed always 1 conn (UNCHANGED) | `crates/common/src/config.rs` (Sub-PR #1) |
| `MAX_DAILY_UNIVERSE_SIZE = 400` constant | Universe size cap; HALT if exceeded | `crates/common/src/constants.rs` (Sub-PR #7) |
| `crates/storage/tests/instance_type_lock_guard.rs` | Source-scan: pins `t4g.large` in `aws-budget.md` + this file + architecture doc §5 | NEW in Sub-PR #1 |
| `crates/storage/tests/daily_universe_scope_guard.rs` | Source-scan: pins Detailed CSV URL + DEDUP key contract + Quote-mode-for-all + retry-policy unbounded loop + I-P1-11 composite key | NEW in Sub-PR #1 |
| Source-scan retirement of `indices4only_scope_lock_guard.rs` | Old ratchet removed | RETIRED in Sub-PR #1 |
| Per-row CSV schema validation | Reject CSV if >0.1% rows fail mandatory-field check | `crates/core/src/instrument/csv_parser.rs` (Sub-PR #4) |
| Lifecycle reconciler test coverage | Idempotent UPSERT + state-flip + dangling-reference rejection | `crates/storage/tests/instrument_lifecycle_*.rs` (Sub-PR #9) |
| RAM-first hot path (UNCHANGED) | banned-pattern scanner blocks SELECT in indicator/strategy/risk paths | already shipped |

---

## §12. What a PR that violates this lock looks like (REJECT)

- Removes the `DailyUniverse` enum variant or adds a parallel variant without a dated operator quote
- Re-introduces `Indices4Only` or `FullUniverse` or `IndicesOnlyAllExpiries` or `IndicesUnderlyingsOnly` variants
- Adds a fallback data source (REST LTP, S3 cache, yesterday's stale CSV) to the instrument-fetch path
- Adds a give-up condition to the fetch retry loop (any code path returning Err without retry)
- Changes the subscription mode for ANY universe SID from Quote to Ticker or Full without a dated operator quote
- Adds a 2nd main-feed connection or any new WS endpoint
- Subscribes the daily universe to derivative contracts (FUTIDX/OPTIDX/FUTSTK/OPTSTK) — only the UNDERLYING_SECURITY_ID spot rows are subscribed
- DELETES rows from `instrument_lifecycle` (lifecycle_state transitions are the ONLY allowed mutation; no DELETE statements)
- Changes `effective_main_feed_pool_size` to anything other than 1
- Modifies instance type from t4g.large without the 4-file update protocol in §7 Mechanical Rule 1

Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this rule file FIRST with a dated quote, only then can the PR land. This prevents accidental scope creep through casual approvals.

---

## §13. Honest 100% claim (mandatory wording per `operator-charter-forever.md` §F)

When any PR / commit message / Telegram body invokes "100% guarantee" for the daily-universe pipeline, it MUST be qualified exactly:

> "100% inside the tested envelope, with ratcheted regression coverage:
> infinite retry on instrument CSV fetch with escalating Telegram (Info→High→Critical at attempts 4/11/21);
> app boot BLOCKS until fresh CSV in hand — never proceeds with stale, partial, or corrupt data;
> ≤20-second tick burst absorbed via 100K-tick rescue ring (constant `TICK_BUFFER_CAPACITY`, ratcheted by `zero_tick_loss_alert_guard.rs`);
> beyond 20s, NDJSON spill → DLQ catches every payload as recoverable text;
> all 21 timeframes RAM-resident for today + yesterday across ~250 SIDs (~800 MB working set on t4g.large 8 GiB host);
> `instrument_lifecycle` table is SEBI-compliant — NO DELETEs ever, only state transitions to `expired_*` SYMBOLs preserved in `(security_id, exchange_segment)` composite-key history per I-P1-11;
> daily lifecycle reconciler captures every appearance, disappearance, rename, segment-move, split — logged to `instrument_lifecycle_audit` forensic chain with 5-year SEBI retention."

Stronger phrasing ("never miss a tick", "WebSocket never disconnects", "QuestDB never fails") without the envelope qualifier = REJECT IN REVIEW.

---

## §14. Auto-driver / Insta-reel explanation

> Sir, imagine the juice shop boy goes to the fruit market every morning at 8:30 AM — BEFORE the shop opens at 9 AM. He picks up the FRESH list of every fruit available today. He throws away yesterday's list. From today's list he picks: (a) every type of basket index — NIFTY, BANKNIFTY, SENSEX, FINNIFTY, all the sectoral baskets, even the one BSE SENSEX basket. (b) For every fruit that has a futures or options contract (apple-Dec-100-Call, mango-Jan-200-Put, etc.), the boy notes the underlying fruit's spot price slot. He doesn't subscribe to every contract — just the underlying spot — about 250 spots total.
>
> If the boy can't get the list (network down, Dhan server down), he keeps trying FOREVER — never gives up, never substitutes a stale list, never makes one up. The phone rings (Telegram) at 5 minutes, then 15 minutes, then every 5 minutes after that — but the shop refuses to open without the fresh list. Better closed than open with the wrong list.
>
> Old fruits that disappear from today's list (e.g., a stock that left the F&O list) stay in the register marked "expired" — never deleted. The register is the SEBI auditor's friend: every appearance, disappearance, rename, split is logged with a timestamp. Five years later we can still answer "was Mazagon Dock in the universe on 27 May 2026?"

---

## §15. Operator decision protocol — how to re-expand or contract the scope

To change the daily universe scope (e.g., add NSE_FNO contracts directly, add commodity, add currency):

1. Operator provides explicit verbatim quote authorizing the expansion.
2. Edit THIS file: add the new instrument class to §2 allowed-set table + update §11 mechanical guards.
3. Edit `operator-charter-forever.md` §I to reflect the new scope.
4. Edit `websocket-connection-scope-lock.md` to update the LOCKED contract.
5. Edit `aws-budget.md` if the change has a cost impact.
6. Update ratchet `crates/storage/tests/daily_universe_scope_guard.rs` to pin the new contract.
7. Open the actual scope-expansion PR citing the rule-file edit commit as its authority.

**No "I think the operator probably meant…" expansions.** This rule file is the single source of truth.

---

## §16. Trigger (auto-loaded paths)

Always loaded. Activates on any session that:
- Edits `crates/app/src/main.rs` (boot sequence)
- Edits any file under `crates/core/src/websocket/`
- Edits any file under `crates/core/src/instrument/`
- Edits `crates/common/src/config.rs` (`SubscriptionScope` or related enums)
- Edits `crates/common/src/locked_universe.rs` (the prior 4-SID const lives here; superseded but file may persist as legacy)
- Edits `config/base.toml` `[subscription]` or `[websocket]` or `[instrument_master]` sections
- Adds any new `wss://` URL constant
- Edits any file under `deploy/aws/`
- Adds any new audit table named `instrument_*_audit`
- Calls any `spawn_*_connection` or `spawn_*_pipeline` function

---

## §17. Cross-references — these files are SUPERSEDED by this file's contract

The following 4 rule files are RETAINED for historical audit (the codebase pattern stacks dated operator-decision sections), but their CURRENT EFFECTIVE CONTRACT is whatever is in this file:

| File | Section superseded |
|---|---|
| `.claude/rules/project/aws-budget.md` | Instance type (t4g.medium → t4g.large), bill (~₹1,022/mo → ~₹1,514/mo), schedule (08:00 → 08:30 IST), memory map (4 GiB → 8 GiB) |
| `docs/architecture/aws-indices-only-locked-architecture.md` §5 | Same instance/bill/schedule/memory updates |
| `.claude/rules/project/websocket-connection-scope-lock.md` | Allowed instruments (4 IDX_I → ~250 daily-universe SIDs), subscription mode (Ticker for IDX_I → Quote for all), `SubscriptionScope` enum variant (`Indices4Only` → `DailyUniverse`) |
| `.claude/rules/project/operator-charter-forever.md` §I | Same WS scope updates |

Each of those files gets a one-line "SUPERSEDED BY daily-universe-scope-expansion-2026-05-27.md (2026-05-27)" marker prepended in Sub-PR #0 so future readers find this file from any entry point.

---

# Sub-PR #1.5 enrichment (2026-05-27) — adversarial 3-agent findings

The 3-agent adversarial review run during Sub-PR #1 (hot-path-reviewer +
security-reviewer + general-purpose hostile bug-hunt) produced 8 CRITICAL,
17 HIGH, 12 MEDIUM and 5 LOW findings. The CRITICAL/HIGH items are
captured below as locked contracts for the subsequent sub-PRs to
implement. Sub-PR #1.5 ships these contract additions as pure markdown
(no Rust code) so the contracts exist BEFORE the implementation PRs land.

---

## §18. CSV downloader hardening contract (Sub-PR #3 must implement)

Addresses security-reviewer findings **S-C1, S-C2, S-M1, S-L1**.

The CSV download client in `crates/core/src/instrument/csv_downloader.rs` (Sub-PR #3) MUST:

| Hardening | Locked value | Why |
|---|---|---|
| **Redirect policy** | `reqwest::redirect::Policy::none()` | A DNS-poisoned response could 301 to attacker host serving malicious CSV with valid TLS for the attacker domain. Refuse to follow ANY redirect. |
| **Response body size cap** | `MAX_CSV_BODY_BYTES = 50 * 1024 * 1024` (50 MB) | Expected size is 5-15 MB. A malicious or malfunctioning server could stream gigabytes before we trigger row-count validation; bound the read explicitly. |
| **Content-Type assertion** | Must be `text/csv` OR `application/octet-stream` OR `text/plain` — REJECT `text/html` (WAF block page), `application/json` (Dhan-side bug serving wrong content) | Avoids feeding a JSON error body to the CSV parser. |
| **Path validation on cache write** | `cache_path.starts_with(CACHE_BASE_DIR).is_ok()` BEFORE write | Defense-in-depth against symlink attacks on `data/instrument-cache/`. |
| **Connect timeout** | `Duration::from_secs(10)` | Avoid hanging on a black-hole DNS. |
| **Read timeout** | `Duration::from_secs(60)` | Bound a single GET attempt. Combined with retry policy §4, total wall-clock is bounded. |
| **No URL logging** | Never log the full URL with query params (none in this case, but defensive) | Defensive — `tracing` field `url = "<dhan_csv>"` only. |

**Ratchets (Sub-PR #3):**

- `crates/core/tests/csv_downloader_redirect_guard.rs::test_csv_downloader_client_has_no_redirect_policy`
- `crates/core/tests/csv_downloader_body_cap_guard.rs::test_csv_download_aborts_at_body_size_limit`
- `crates/core/tests/csv_downloader_content_type_guard.rs::test_csv_download_rejects_html_response`
- `crates/storage/tests/daily_universe_scope_guard.rs::daily_universe_csv_hardening_constants_pinned`

---

## §19. Boot-step deadlines + EC2 cron heartbeat (Sub-PR #2 must implement)

Addresses hostile findings **O-C1, O-C4**.

The infinite-retry policy in §4 applies ONLY to the CSV fetch path. Pre-CSV boot steps (auth, QuestDB DDL, IP whitelist verification) have their OWN deadlines:

| Boot step | Deadline | On exceeded |
|---|---|---|
| Step 1-5 (config / observability / logging / notification) | 30s | Severity::High → operator notified, retry once |
| Step 6 (Dhan auth — TOTP → JWT) | 60s total (3 × 20s retries) | Severity::Critical → BOOT BLOCKS, operator paged |
| Step 6a (Dhan static-IP whitelist GET `/v2/ip/getIP`) | 30s | Severity::Critical → BOOT BLOCKS |
| Step 6b (QuestDB DDL — including new `instrument_lifecycle` + `instrument_lifecycle_audit` tables) | 60s | Severity::Critical → BOOT BLOCKS (BOOT-01 / BOOT-02) |
| Step 6c (Daily universe orchestrator — §10 algorithm) | infinite retry per §4 | Per §4 escalation table |

**EC2 cron heartbeat (out-of-band detection):**

A CloudWatch Events scheduled rule fires at 08:40 IST every day:
- IF `tv_boot_completed` metric is missing in the last 10 minutes
- THEN trigger Lambda → SNS Critical: "EC2 failed to start OR app failed to boot — investigate"

This catches the case where EventBridge fails to fire the cron OR the EC2 instance never starts (AWS capacity exhaustion). Without this, the §4 infinite-retry policy is moot because the app never gets to start retrying.

**Ratchets (Sub-PR #2):**

- `crates/app/tests/boot_step_timeout_guard.rs::test_each_boot_step_has_a_deadline_constant`
- `crates/app/tests/boot_step_timeout_guard.rs::test_step_6_auth_deadline_is_60s`
- `deploy/aws/cloudwatch-alarms/boot-heartbeat-alarm.tf` + `crates/storage/tests/cloudwatch_boot_heartbeat_guard.rs::test_cloudwatch_boot_heartbeat_alarm_exists`

---

## §20. Operator escape valve — `--operator-acknowledge-stale-csv <sha256>` (Sub-PR #2)

Addresses hostile finding **O-C3** (SHA-256 reject loop deadlock).

The §4 infinite-retry policy means a permanently corrupt Dhan CSV (e.g. their CDN cache poisoning, Dhan-side bug serving the same broken file) blocks boot FOREVER with no escape. This is correct safety semantics but leaves operator without a manual override.

**The ONE override path** (no other escape valve, ever):

```
tickvault --operator-acknowledge-stale-csv <expected_yesterday_sha256> [other flags...]
```

Semantics:
1. Operator MUST paste yesterday's SHA-256 explicitly. Can NOT be silently triggered.
2. App reads yesterday's `instrument_lifecycle` table snapshot.
3. App verifies `expected_yesterday_sha256` matches what's in yesterday's `instrument_fetch_audit.csv_sha256`. If mismatch → reject the flag, continue infinite retry.
4. If match → app proceeds with yesterday's lifecycle as ground truth FOR ONE TRADING DAY ONLY.
5. Write a `instrument_fetch_audit` row with `outcome = stale_csv_operator_override`, `operator_note = <required free text>`.
6. Telegram Severity::Critical: "OPERATOR OVERRIDE — running with yesterday's CSV. Investigate Dhan CSV corruption."
7. App boot continues; subscription dispatched against yesterday's universe.

**On the NEXT day's boot:** the flag is NOT remembered. Operator must either fix the upstream issue OR re-pass the flag with FRESH SHA-256.

**Ratchets (Sub-PR #2):**

- `crates/app/tests/operator_override_csv_guard.rs::test_operator_override_requires_yesterday_sha256_match`
- `crates/app/tests/operator_override_csv_guard.rs::test_operator_override_writes_critical_telegram`
- `crates/app/tests/operator_override_csv_guard.rs::test_operator_override_does_not_persist_to_next_day`

---

## §21. Sub-PR ordering safety — compile-time feature gate

Addresses hostile finding **O-C2** (PR ordering safety).

If Sub-PR #7 (default flip to `DailyUniverse`) merges BEFORE Sub-PR #3 (CSV downloader) + #4 (parser) + #9 (lifecycle reconciler), the app would boot with `DailyUniverse` scope but no fetcher implementation — INSTR-FETCH-01 Critical forever, unrecoverable.

**Mechanical guard (added in Sub-PR #1.5 as a contract; code lands in Sub-PR #3+):**

```toml
# crates/common/Cargo.toml
[features]
default = []
daily_universe_fetcher = []  # toggled on by Sub-PR #9 ONLY
```

The `DailyUniverse` variant currently shipped in Sub-PR #1 (PR #810) is gated by `#[cfg(feature = "daily_universe_fetcher")]` in Sub-PR #3 onward, so production code can't switch to it until the fetcher exists.

**Sequencing locked:**
- Sub-PRs #1, #1.5, #2, #6 (rule-file + boot-orchestrator scaffolding) — no feature flag toggle
- Sub-PRs #3, #4 (downloader + parser) — code lives behind the feature flag
- Sub-PR #9 (lifecycle reconciler) — flips `daily_universe_fetcher = ["..."]` on
- Sub-PR #7 (default flip) — now safe to land
- Sub-PR #10 (boot orchestrator wiring) — references the variant unconditionally

**Ratchets (Sub-PR #3):**

- `crates/common/src/config.rs::tests::test_daily_universe_variant_only_compiles_with_feature_flag`
- `crates/storage/tests/daily_universe_feature_flag_guard.rs::test_feature_flag_not_default_enabled`

---

## §22. Holiday / Muhurat / declared-holiday handling (Sub-PR #2 must implement)

Addresses hostile findings **O-H1, O-H4** (holiday CSV thrash + Muhurat trading).

The daily orchestrator MUST consult `TradingCalendar::is_holiday(today_ist)` BEFORE entering the §4 infinite-retry loop:

| Day classification | Orchestrator behaviour |
|---|---|
| Regular trading day | Full §4 algorithm — fetch, validate, build universe, subscribe |
| Saturday / Sunday | Single fetch attempt with 30s timeout. If success: noop log (SHA-256 likely matches yesterday). If failure: log Severity::Info, exit clean. No subscription dispatch (market closed). |
| Declared holiday (Republic Day / Independence Day / Diwali Laxmi Pujan etc.) | Same as Sat/Sun |
| Muhurat trading day (Diwali) | Special evening start — EC2 cron OVERRIDES from 08:30 morning to 17:30 IST (1 hour before Muhurat session). Operator must pre-populate the calendar OR pass `--muhurat-mode 18:00` flag. |
| End-of-fiscal-year (Mar 31) | Normal day — Dhan publishes normal CSV; nothing special. |

**Why this matters:** infinite retry against a holiday's stale Dhan CSV would hammer their CDN at 12 GETs/hour × 24 hours = ~280 GETs and risks WAF rate-limit ban.

**Ratchets (Sub-PR #2):**

- `crates/core/tests/holiday_orchestrator_guard.rs::test_holiday_orchestrator_skips_retry_loop_on_declared_holiday`
- `crates/core/tests/holiday_orchestrator_guard.rs::test_holiday_orchestrator_runs_normal_on_trading_day`
- `crates/core/tests/holiday_orchestrator_guard.rs::test_muhurat_mode_flag_overrides_cron`

---

## §23. Stock-split / symbol-rename / segment-move classification (Sub-PR #9)

Addresses hostile findings **O-H2** (stock split) + **O-H3** (rename chain).

The §6 `transition_kind` SYMBOL enum is extended:

| New value | Trigger | Audit row payload |
|---|---|---|
| `split` | `lot_size_new < lot_size_old × 0.5` OR `tick_size_new < tick_size_old × 0.5` | `field_deltas = {"lot_size": [old, new], "tick_size": [old, new]}`, Telegram Severity::High "Stock split: TCS old_lot=1000 new_lot=200" |
| `segment_moved` | Same `security_id` appears under a different `exchange_segment` than yesterday | Severity::High Telegram. New row in lifecycle (composite key includes segment, so it's a DISTINCT row), old row marked `expired_*`. |

`prev_symbol` upgraded from SYMBOL → STRING (JSON array, append-only):

```sql
ALTER TABLE instrument_lifecycle DROP COLUMN prev_symbol;
ALTER TABLE instrument_lifecycle ADD COLUMN prev_symbol_chain STRING;
```

Example chain: `["HDFC", "HDFCBANK_TMP"]` after two renames.

**Ratchets (Sub-PR #9):**

- `crates/core/tests/lifecycle_split_detection_guard.rs::test_lot_size_half_or_less_classified_as_split`
- `crates/core/tests/lifecycle_split_detection_guard.rs::test_split_writes_high_severity_telegram`
- `crates/core/tests/lifecycle_rename_chain_guard.rs::test_three_hop_rename_preserves_full_chain`
- `crates/core/tests/lifecycle_segment_move_guard.rs::test_segment_move_creates_new_row_and_expires_old`

---

## §24. Audit-chain ordering — write-audit-first (Sub-PR #9)

Addresses hostile finding **O-H7** (partial audit-chain write on QuestDB ILP error).

If the orchestrator UPSERTs `instrument_lifecycle` and then crashes BEFORE INSERTing `instrument_lifecycle_audit`, the forensic chain is broken.

**Locked write order (Sub-PR #9):**

```
For each lifecycle transition:
  1. INSERT INTO instrument_lifecycle_audit (transition_kind = "pending", ...)
  2. UPSERT INTO instrument_lifecycle (...)
  3. UPDATE instrument_lifecycle_audit SET transition_kind = "<actual_kind>" WHERE id = <step1_id>
```

If step 2 or 3 fails, we have a `pending` audit row recording the attempt — better than no audit at all.

**Ratchet (Sub-PR #9):**

- `crates/storage/tests/lifecycle_audit_ordering_guard.rs::test_audit_row_written_before_lifecycle_upsert`

---

## §25. Point-in-time SEBI audit reconstruction (Sub-PR #9)

Addresses hostile finding **O-H6**.

SEBI auditor query: "what was the universe on 2026-05-27?"

Today's `instrument_lifecycle` is overwrite-on-UPSERT (only LATEST state per `(security_id, exchange_segment)`). The forensic chain in `instrument_lifecycle_audit` must support point-in-time queries.

**Schema additions (Sub-PR #9):**

`instrument_lifecycle_audit` gains columns capturing the post-transition state snapshot:

- `lifecycle_state_after` SYMBOL
- `lot_size_after` INT
- `tick_size_after` DOUBLE
- `expiry_date_after` TIMESTAMP
- `symbol_name_after` SYMBOL

Cost: +200 bytes/row × ~50 transitions/day × 5 years ≈ 18 MB total. Trivial.

**Example query:**

```sql
SELECT lifecycle_state_after, symbol_name_after
FROM instrument_lifecycle_audit
WHERE security_id = X AND exchange_segment = 'NSE_EQ' AND ts <= '2026-05-27T15:30:00Z'
ORDER BY ts DESC LIMIT 1;
```

**Ratchet (Sub-PR #9):**

- `crates/storage/tests/lifecycle_audit_pit_query_guard.rs::test_point_in_time_query_returns_state_at_date`

---

## §26. CSV parser robustness — BOM / line endings / quoted commas (Sub-PR #4)

Addresses hostile finding **O-H8** + security-reviewer **S-L2** (BiDi unicode).

The CSV parser MUST handle:

| Edge case | Behaviour |
|---|---|
| BOM (`\xEF\xBB\xBF`) at file start | Strip silently |
| CRLF / LF / CR mixed line endings | Normalize to LF |
| Quoted field with embedded comma `"REL,IND"` | Parse correctly via `csv` crate `Trim::All` mode |
| UTF-8 strict | Reject ISO-8859-1 bytes outside 7-bit ASCII |
| BiDi override characters in `symbol_name` | Strip via `sanitize_audit_string` (already in `crates/common/src/sanitize.rs`) |
| Row with >0.1% mandatory-field failures | Reject whole CSV per §3 |
| Single-row corruption ≤0.1% | Drop the row, log Severity::High, continue |
| Dangling `UNDERLYING_SECURITY_ID` reference (no NSE_EQ row matches) | Threshold > 0.5% derivative rows → reject CSV; below → drop the affected derivative rows + log Severity::High |

**Ratchets (Sub-PR #4):**

- `crates/core/tests/csv_parser_robustness_guard.rs::test_parser_strips_utf8_bom`
- `crates/core/tests/csv_parser_robustness_guard.rs::test_parser_normalizes_mixed_line_endings`
- `crates/core/tests/csv_parser_robustness_guard.rs::test_parser_handles_quoted_commas`
- `crates/core/tests/csv_parser_robustness_guard.rs::test_parser_rejects_iso_8859_1`
- `crates/core/tests/csv_parser_robustness_guard.rs::test_parser_strips_bidi_overrides_in_symbol_name`
- proptest: `crates/core/tests/csv_parser_property.rs::arbitrary_mutations_either_parse_or_reject_cleanly`

---

## §27. Dry-run universe isolation (Sub-PR #10)

Addresses hostile finding **O-H9**.

The `--dry-run-universe` flag (per option Z approval) writes `instrument_lifecycle` + `instrument_fetch_audit` + `instrument_lifecycle_audit` rows. Without isolation, a Day-1 dry-run leaks into Day-2 live run as ground truth.

**Schema additions (Sub-PR #9 / #10):**

Both `instrument_lifecycle` and `instrument_lifecycle_audit` gain:

- `dry_run` BOOLEAN

Daily orchestrator reads ONLY `WHERE dry_run = false` rows for delta computation. Lifecycle reconciler writes the column value matching the orchestrator's runtime mode.

**Ratchet (Sub-PR #10):**

- `crates/core/tests/dry_run_isolation_guard.rs::test_dry_run_rows_not_used_for_next_day_delta`

---

## §28. Operator boundary — indicators + strategies OFF LIMITS (operator lock 2026-05-27)

Operator directive verbatim 2026-05-27:
> "as of now don't even touch indicators and strategies area dude okay?"

**Effect on this plan:**

- The hot-path agent's 2 CRITICAL findings (C1: indicator warmup gate, C2: I-P1-11 violation in `IndicatorEngine::states` flat-Vec) are TRACKED but will NOT be remediated in the 14-sub-PR sequence.
- Sub-PR #8 (was "21-TF aggregator + indicator engine RAM-cache scale-up") is RE-SCOPED to **aggregator-only**. No indicator changes.
- Any future PR proposing changes under `crates/trading/src/indicator/` or `crates/trading/src/strategy/` requires a fresh dated operator approval.
- The known I-P1-11 violation in `IndicatorEngine::states` is documented here as a KNOWN GAP — operator-acknowledged risk.

**Why this is honest:** under `Indices4Only` (current default) the gap doesn't manifest because the 4 IDX_I SIDs don't collide. Under `DailyUniverse` the gap WOULD manifest — but `DailyUniverse` is gated behind the feature flag (§21) until operator removes the indicator/strategy boundary.

**Ratchet (Sub-PR #1.5):**

- `crates/storage/tests/operator_boundary_indicator_strategy_guard.rs::test_indicator_engine_states_field_unchanged_since_2026_05_27`
  (source-scan: SHA-256 the relevant lines of `engine.rs`; any modification fails the build until the boundary is lifted)
