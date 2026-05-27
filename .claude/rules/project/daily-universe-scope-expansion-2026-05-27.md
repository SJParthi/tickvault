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
