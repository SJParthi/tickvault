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

**Quote 5 (2026-05-29, instance = m8g.large + weekday-only schedule):**
> "why m8 why not c8 or r8 dude?" … "see only on tarding working days our plan is to run only 8.30 am till 4.30 pm max dude … then after that whenever manually i need to run the isnatcne i will do it"
>
> Operator authorized: (a) instance lock → **m8g.large** (Graviton4, latest gen, 2 vCPU / 8 GiB, general-purpose — m-family chosen over c8g [4 GiB, too little RAM] and r8g [16 GiB, wasteful] because 8 GiB needs the 4:1 general-purpose ratio); live ap-south-1 on-demand = **$0.06416/hr**. (b) Schedule → **trading weekdays only (Mon–Fri), 08:30–16:30 IST auto start/stop**; operator starts manually for any out-of-window run. This is the dated quote §7 Mechanical Rule 1 + §12 require before the instance + schedule change.

**Quote 6 (2026-05-29, EBS + no EIP + <₹2,000 target for data-pull):**
> "for storage atleast we need 100 gb right so even in later phase we can easily move everything to s3 right dude" … "why ebs is so costly why not internal storage dude?" … "it should be less than 2000 dude thats our plan" … "exclude static ip dude since for the next three months no real orders … lets say 270 hours"
>
> Operator authorized + resolved: (c) EBS gp3 **30 GB** (not 100 — 100 GB pushed the all-in bill to ~₹2,698 incl GST, over the operator's <₹2,000 target; 30 GB hot window + S3 archival of >90d partitions keeps it ~₹2,058. Internal/instance NVMe rejected — wiped on daily stop). gp3 grows online; raise anytime. (d) **Elastic IP excluded** (`enable_eip = false`) for the 3-month no-orders data-pull — saves ~₹430/mo; re-enable before live trading. (e) cost basis = **270 running hrs/month**. (f) Tax = **18% GST** total (IGST inter-state, or CGST 9% + SGST 9% intra-state — same 18%, no extra cess). All-in ≈ **₹2,058/mo incl. GST** (~₹58 over the ₹2,000 target — operator accepted; drop to 20 GB for strictly-under).

**Quote 7 (2026-06-30, instance = r8g.large 16 GiB — supersedes the 2026-05-29 m8g.large lock):**
> "just upgrade the instance to r8 large + everything related to the infra"
>
> Operator authorized: the host instance upgrades from **m8g.large** (2 vCPU /
> 8 GiB, general-purpose) → **r8g.large** (Graviton4, 2 vCPU / **16 GiB**,
> memory-optimized r-family) — DOUBLING RAM for the upcoming both-feeds +
> larger-universe workload. Live ap-south-1 on-demand = **$0.08258/hr**. The
> **Elastic IP is KEPT** (`enable_eip = true`, the 2026-05-31 flip — without it
> the box has NO public IP after a stop/modify/start, so it could reach neither
> SSM nor Dhan; see Terraform `enable_eip` description). Everything related to
> the infra moves to r8g.large in lockstep per §7 Mechanical Rule 1: this rule
> file §7, the superseded `aws-budget.md` + `aws-indices-only-locked-architecture.md`
> §5 markers, the `instance_type_lock_guard.rs` ratchet, the Terraform validation,
> the upgrade-script `FROM_TYPE` default (now `m8g.large`), and the Docker
> QuestDB `QDB_MEM_LIMIT` default (2g → 4g, comfortable in 16 GiB). This dated
> quote satisfies §7 Mechanical Rule 1 for the instance change.
> **Post-resize note (2026-07-01):** the resize is now DONE (box is physically
> `r8g.large`/16 GiB, effective at the 08:30 IST auto-start), so the docker-compose
> `mem_limit: ${QDB_MEM_LIMIT:-4g}` default is now 4g directly — no longer coupled
> to `scripts/aws-upgrade-instance.sh` (which #1274/#1278 had used to keep the
> compose default at 2g until the physical resize). The 2K-universe
> expansion (`MAX_DAILY_UNIVERSE_SIZE` / `SEAL_BUFFER_CAPACITY`) is a SEPARATE
> later PR gated on a live memory measurement and is NOT changed here;
> `dry_run` stays `true`.

**Approvals:**
- 2026-05-27: Approved Sub-PR plan items A–D (infinite retry policy, single `instrument_lifecycle` table, separate `instrument_lifecycle_audit` table, plan growing to 14 sub-PRs)
- 2026-05-27: Approved options X–Z (EventBridge cron at 08:30 IST, `lifecycle_state_locked` column for operator overrides, `--dry-run-universe` CLI flag for first prod validation)
- 2026-05-29: Approved instance m8g.large (Graviton4, 8 GiB) + weekday-only 08:30–16:30 IST auto schedule (manual start otherwise) per Quote 5
- 2026-05-29: Approved EBS 100 GB + EIP excluded (no orders) + 270-hr basis → ~₹2,698/mo incl GST per Quote 6
- 2026-06-02: Operator WIDENED the schedule from 08:30–16:30 IST to **08:00–17:00 IST** (verbatim: "instead of 8.30 am make it as 8 am till 5 pm dude so that pre-market and post-market and deployment and all other activities can run without any worries"). Crons: start `cron(30 2 ? * MON-FRI *)` (02:30 UTC = 08:00 IST), stop `cron(30 11 ? * MON-FRI *)` (11:30 UTC = 17:00 IST). Cost: +1 hr/day (~+₹120/mo), still inside the ~₹2,058/mo envelope. This dated quote satisfies §7 Mechanical Rule 1 for the schedule change.
- 2026-06-05: Operator NARROWED the schedule back from 08:00–17:00 IST to **08:30–16:30 IST** (verbatim: "make the aws instance start and stop from 8.30 am till 4.30 pm dude one and only when it is needed let me start it manually"). Crons: start `cron(0 3 ? * MON-FRI *)` (03:00 UTC = 08:30 IST), stop `cron(0 11 ? * MON-FRI *)` (11:00 UTC = 16:30 IST). The start-watchdog ping/check move to 08:30/08:45 IST; the GitHub-Actions after-close start cron + `aws-autopilot.sh`/`deploy-aws.yml` up-window move to 08:30–16:30 in lockstep. Cost: −1 hr/day (~−₹120/mo). The 08:30 start still gives the documented §10 boot budget before the 09:00 pre-open. This dated quote satisfies §7 Mechanical Rule 1 + §12 for the schedule change and supersedes the 2026-06-02 widening.
- 2026-06-30: Approved instance UPGRADE m8g.large → **r8g.large** (Graviton4, 2 vCPU / 16 GiB) + EIP KEPT per Quote 7; bill ~₹2,058/mo → ~₹2,919/mo incl GST (270 hrs, 30 GB EBS, +EIP). The 2K-universe expansion is deferred to a separate later PR.
- 2026-07-13: Approved EBS grow 30 GB -> 50 GB gp3 (+~₹170/mo incl GST) + instance-role S3 write on tv-prod-cold/groww-capture/* (Groww capture archival), per operator quote: "go ahead and merge everything once it is green - yes do whatever is the recommendation" (prod disk-pressure remediation - disk hit 82% on 2026-07-13 with zero reclamation). Note: the instance role's existing cold-bucket statement (main.tf, s3:GetObject/PutObject/ListBucket on the whole `tv-<env>-cold` bucket) ALREADY covers the groww-capture/* prefix — no IAM change was needed. Bill ~₹2,919/mo → ~₹3,101/mo incl GST (recomputed below).

---

## §1. The rule — one paragraph

This product, starting from the date of this lock, opens exactly TWO WebSocket connections to Dhan FOREVER (unchanged from prior lock): one main-feed + one order-update. The main-feed connection subscribes to a **daily universe** of approximately **250 instrument SecurityIds** — **all NSE indices in IDX_I segment + 1 BSE SENSEX in IDX_I segment + the unique F&O underlying spot instruments (NSE_EQ)** — pulled fresh every trading morning at 08:45 IST from Dhan's static Detailed CSV. Every subscription is in **Quote mode** (Feed Request Code 17, 50-byte response packets carrying day OHLC). The previous 4-SID `LOCKED_UNIVERSE` const + `SubscriptionScope::Indices4Only` single-variant enum + the 4-IDX_I-only ratchet are RETIRED. The new compile-time enum variant is `SubscriptionScope::DailyUniverse`. The host instance upgrades from t4g.medium 4 GiB → **t4g.large 8 GiB** to hold today + yesterday sealed bars across all 21 timeframes for ~250 SIDs in RAM.

---

## §2. The complete allowed set (POST-2026-05-27)

| WebSocket | Count | Endpoint | Allowed instruments | Mode |
|---|---|---|---|---|
| **Main feed** | **1** | `wss://api-feed.dhan.co?version=2&token=<JWT>&clientId=<ID>&authType=2` | Daily-fetched universe (~250 SIDs): all `IDX_I` rows where `EXCH_ID IN (NSE, BSE)` AND `INSTRUMENT == INDEX` (~30) + every unique `UNDERLYING_SECURITY_ID` referenced by `FUTIDX/OPTIDX/FUTSTK/OPTSTK` rows, resolved to its NSE_EQ row (~218) + ALL available monthly FUTIDX expiries of the 4 underlyings (NIFTY/BANKNIFTY/MIDCPNIFTY = NSE_FNO, SENSEX = BSE_FNO; typically ~12 contracts, envelope ≤24) per §36/§36.7 (2026-07-10) | **Quote (request code 17)** — 50-byte packets, gives day OHLC at fixed byte offsets |
| **Order update** | **1** | `wss://api-order-update.dhan.co` | Receives order events for orders WE place; filter `Source=P` | JSON, MsgCode 42 auth |

**Total live WebSocket connections to Dhan: 2** (UNCHANGED from prior lock).

**Universe size envelope (mechanical bound):** `MAX_DAILY_UNIVERSE_SIZE = 1200` (raised from 400 per §31, NTM expansion 2026-06-06). Boot HALTS if computed universe is outside `[100, 1200]`. Fits comfortably on 1 main-feed connection (Dhan cap = 5,000 SIDs/conn). The §36.7 FUTIDX all-months grant (2026-07-10) adds the vendor-listed monthly serials (~12 SIDs typical, ≤24 by envelope) to the subscription set — still trivially inside `[100, 1200]` (≈343 total).

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

**Quote 5 (2026-05-29, applicable-F&O master — supersedes the §10-step-4 "indices + underlyings only" scope for the lifecycle table):**
> "I asked you to pull ALL the FNO in instruments … only fno for our applicable fno instruments right dude … if yes go ahead"

**MASTER vs SUBSCRIPTION (locked 2026-05-29):** `instrument_lifecycle` is the **full applicable-F&O master** — it stores, in addition to the indices + F&O underlying spots, **every applicable F&O contract**: the `FUTSTK`/`OPTSTK` rows whose `UNDERLYING_SECURITY_ID` resolves to one of our tracked NSE_EQ underlyings, plus the `FUTIDX`/`OPTIDX` rows for our tracked indices. Currency F&O (`FUTCUR`/`OPTCUR`), commodity F&O (`FUTCOM`/`OPTFUT`), and non-F&O equities NOT in our underlying set are EXCLUDED. These contract rows carry `lifecycle_state` transitions (`active` → `expired_contract`) and are NEVER deleted (SEBI §25 point-in-time). **This is the master/audit table ONLY — it does NOT change the WebSocket subscription**, which remains the 331-SID indices+spots set per §2 **plus the §36.7 all-monthly-expiries FUTIDX contracts of the 4 underlyings (2026-07-10; the nearest expiry is the first of each set)** + the 2-WebSocket lock. The `MAX_DAILY_UNIVERSE_SIZE = 1200` envelope in §2 bounds the *subscription* set, NOT the lifecycle master (which legitimately holds ~219K applicable-F&O rows).

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

## §7. Instance lock — r8g.large (LOCKED 2026-06-30, supersedes the 2026-05-29 m8g.large + 2026-05-27 t4g.large + 2026-05-18 t4g.medium locks)

**2026-06-30 change (operator Quote 7):** instance lock → **r8g.large**
(Graviton4, the latest generation, memory-optimized) — **16 GiB RAM** (DOUBLED
from the m8g.large 8 GiB) so the Mechanical Rule 2 memory budget below is
recomputed for 16 GiB. Family choice rationale: the upcoming both-feeds +
larger-universe workload wants headroom, and the **r-family 8:1 (memory)
ratio** gives 16 GiB at the same 2 vCPU — m8g.large (8 GiB, general-purpose
4:1) was the prior lock; c8g.large (4 GiB, compute 2:1) is too little.
r8g.large is the right family/size for 2 vCPU / 16 GiB. EBS-backed (NOT the
`r8gd` local-SSD variant — that NVMe is wiped on every daily auto-stop).

| Spec | Value |
|---|---|
| Instance | **r8g.large** — ARM Graviton4, **2 vCPU, 16 GiB RAM** (memory-optimized) |
| Region | ap-south-1 (Mumbai) |
| Tenancy | Default (Shared) |
| Pricing | On-demand **$0.08258/hr** (live ap-south-1, 2026-06-30) — no Reserved / Savings Plan / Spot |
| Schedule | **Trading weekdays only (Mon–Fri), 08:30–16:30 IST auto** (start `cron(0 3 ? * MON-FRI *)`, stop `cron(0 11 ? * MON-FRI *)`) — narrowed back from 08:00–17:00 on 2026-06-05 per operator ("make the aws instance start and stop from 8.30 am till 4.30 pm"; supersedes the 2026-06-02 widening). Out-of-window runs = operator manual start. Weekends + holidays = OFF unless manually started. |
| EBS | gp3 **50 GB** (2026-07-13 grow; was 30) |
| EIP | 1 (24/7) — **KEPT** (`enable_eip = true`, 2026-05-31 flip; without it the box has no public IP after a stop/modify/start → unreachable by SSM + Dhan) |
| Network | ENA enabled by default |

### Cost bill (LOCKED ~₹3,101/mo incl. 18% GST — 270 hrs, 50 GB EBS, +EIP; was ~₹2,919 at 30 GB pre-2026-07-13)

Operator-set ceiling **270 running hours/month** (auto weekday schedule
~176 hrs + manual runs). **EBS 50 GB** (2026-07-13 grow; was 30 per Quote 6). **Elastic IP KEPT**
(`enable_eip = true`, 2026-05-31 — the box needs a public IP at all). r8g.large
@ $0.08258/hr, $1 ≈ ₹85. **Every running component is itemised below —
monitoring, alerting, Docker, Lambdas, Telegram are all included and
free-tier.**

| Line | Calc | USD |
|---|---|---|
| EC2 r8g.large (hosts app + Docker + QuestDB) | $0.08258/hr × 270 hrs | $22.30 |
| Elastic IP (24/7, KEPT) | $0.005/hr × 720 hrs | $3.60 |
| EBS gp3 50 GB (2026-07-13 grow; was 30 → $2.74) | $0.0912 × 50 | $4.56 |
| S3 cold (aged-out partitions) | tiny, grows over time | $0.18 |
| Docker (QuestDB + tickvault containers) | runs on the EC2 host | $0.00 |
| CloudWatch metrics (10 custom) | free tier = 10 | $0.00 |
| CloudWatch alarms (10) | free tier = 10 | $0.00 |
| CloudWatch Logs (app/journal) | free tier = 5 GB/mo | $0.00 |
| CloudWatch Dashboards (3) | free tier = 3 | $0.00 |
| Lambda (telegram-webhook, budget-killswitch, triage) | free tier = 1M req/mo | $0.00 |
| SNS → Telegram + Email fan-out | free tier (1M / 1k) | $0.00 |
| SNS → SMS (optional) | ~100 India msgs | $0.28 |
| Data transfer out | ~14 GB < 100 GB free egress | $0.00 |
| **Subtotal (pre-GST)** | | **$30.92** |
| **× ₹85/$** | | **₹2,628** |
| **+ 18% GST (AWS India)** | | **~₹3,101/mo** |

**Honest envelope:** ~**₹3,101/month all-in including GST** at the 270-hr
ceiling, 50 GB EBS, with the EIP kept. The 2026-07-13 EBS grow adds ~₹180/mo
(the EBS line moves $2.74 → $4.56). The r8g.large upgrade added ~₹420/mo over
the prior m8g.large bill (the EC2 line moves $17.32 → $22.30) and the EIP adds
~₹306/mo vs the superseded EIP-excluded figure — the pre-grow bill was ~₹2,919
vs the prior ~₹2,058. **The entire observability stack — CloudWatch
metrics/alarms/logs/dashboards, all 3 Lambdas, and Telegram + Email alert
fan-out — costs ₹0** (low-volume control-plane services sit inside AWS's
permanent free tier per §7 Rule 5's CloudWatch-only design; only optional SMS
is ~₹24). 30 GB was originally chosen over 100 GB because the partition manager
archives >90d data to S3 (~4× cheaper/GB), so EBS holds only the hot window;
gp3 grows online if needed — exercised 2026-07-13 with the 30→50 grow when the
root fs hit 82% before any partition reached the 90-day archival age. The EIP is kept because an `aws ec2 modify-instance-attribute`
instance-type flip (stop→modify→start) leaves the ENI with NO ephemeral public
IP (auto-assign-public-IP is a fresh-launch-only attribute), so only the EIP
gives the box an internet path to SSM + Dhan. **Tax:** 18% GST total (IGST
inter-state, or CGST 9% + SGST 9% intra-state — identical 18%, no extra cess).
Verified: r8g.large $0.08258/hr (ap-south-1, 2026-06-30); EIP/EBS/S3/SNS are
AWS list rates. Budget alarm ceiling = $35/mo pre-GST. Operator approved
2026-06-30.

> **Note on instance schedule (2026-05-29):** trading WEEKDAYS only
> (Mon–Fri), **08:30–16:30 IST** auto start/stop. Weekends + NSE holidays
> = instance OFF unless the operator manually starts it. The 08:30 start
> gives the cold-boot + Step 1–6 auth + 08:45 CSV-fetch retry budget so
> the app is ready before 09:00 market open; 16:30 stop is ~1h after the
> 15:30 close (covers post-close digest + flush). Earlier plans
> (08:00–17:00 7-day, then 08:30–17:00 7-day) are superseded.

### Mechanical Rules (replaces aws-budget.md mechanical rules 1+6)

1. **Instance type is r8g.large. PERIOD.** Changing it (to m8g.large, t4g.large,
   m7g, a larger r8g size, etc.) requires:
   - Operator explicit approval with dated quote (see §0 Quote 7)
   - Update to this file
   - Update to `aws-indices-only-locked-architecture.md` §5
   - Update to `aws-budget.md` (existing file marked SUPERSEDED)
   - Ratchet test `crates/storage/tests/instance_type_lock_guard.rs` updated to pin the new type
   - Update to `deploy/aws/terraform/variables.tf` `instance_type` default + validation
   - Update to `scripts/aws-upgrade-instance.sh` `FROM_TYPE` default

2. **Host memory budget for r8g.large (16 GiB total) — POST CloudWatch-only migration, current universe (~250–1000 SIDs across 21 TFs):**
   - QuestDB process: ~4 GB (`QDB_MEM_LIMIT=4g`; write pressure ~5000 ticks/sec sustained, ~2500 audit rows/min)
   - Tickvault app: registry + 21-TF aggregator + indicator state + today/yesterday RAM-resident sealed bars (≈3.2 MB × up to ~1000 SIDs) ≈ **~3.2 GB**
   - App: rescue ring (100K tick cap, fixed): 10 MB
   - App: QuestDB ILP write buffer: 25 MB
   - App: 15+ audit-table buffers: 30 MB
   - Tracing / errors.jsonl rotation buffer: 100 MB
   - OS + FS cache + kernel TCP buffers: ~800 MB
   - **Total used: ~8.2 GB**
   - **Headroom: ~7.8 GB** — well above the 1 GB Linux kswapd floor; the doubled RAM is the reason for the r-family upgrade. (The 2K-universe expansion is a SEPARATE later PR — at ~2K SIDs the app working set grows toward ~6.4 GB and is re-measured before go-live.)

3. **EBS = 50 GB gp3** (operator approval 2026-07-13 — "go ahead and merge everything once it is green - yes do whatever is the recommendation", prod disk-pressure remediation: root fs hit 82% on 2026-07-13 growing ~2.5–3.6 GB/day with zero reclamation. History: 10 GB → 30 GB per the 2026-05-29 Quote 6 lock [30 chosen over 100 to keep the then-bill ~₹2,058/mo near the <₹2,000 target] → 50 GB 2026-07-13). The partition manager auto-archives partitions >90d to the S3 cold bucket (~4× cheaper per GB than EBS), so EBS holds only the hot window. gp3 **grows online** (no stop, no data loss) — 50 GB is a floor; raise it live if the hot window grows. Internal/instance (m8gd local NVMe) storage is NOT used — it is wiped on every daily auto-stop, so it cannot hold QuestDB data. Terraform `ebs_gp3_size_gb` default = 50, range 10–200; the LIVE volume is grown out-of-band (`scripts/aws-upgrade-instance.sh --ebs-size 50`, online `aws ec2 modify-volume`) because `root_block_device[0].volume_size` is in the instance `lifecycle.ignore_changes` — `terraform apply` never touches the live volume.

4. **No paid AWS services** (RDS, ElastiCache, NAT Gateway, ALB) without budget review.

5. **CloudWatch is the sole observability layer** — within free tier (10 metrics + 10 alarms + 5 GB logs).

6. **RAM-first hot path (mandatory, unchanged):** every indicator + strategy + risk decision reads from RAM. QuestDB is persistence + audit + cold-path boot rehydration only. Banned-pattern scanner enforces.

7. **One-time instance upgrade script:** `scripts/aws-upgrade-instance.sh` performs the in-place instance flip (the already-running instance → **r8g.large**, via `--from m8g.large --to r8g.large`) using `aws ec2 stop-instances` → `aws ec2 modify-instance-attribute` → `aws ec2 start-instances`. EIP + EBS preserved (the script verifies the EIP survives and aborts if it changed) — the EIP is mandatory because the stop/modify/start leaves the ENI with no ephemeral public IP. Downtime ~3 minutes, run on a Sunday off-market window. `FROM_TYPE`/`TO_TYPE` in the script are the single source for the flip; the market-hours guard refuses 09:00–15:30 IST Mon–Fri without `--force`.

---

## §8. Subscription mode — Quote for every SID (LOCKED per operator Quote 2)

Every SID in the daily universe — indices, F&O underlyings, **and the §36/§36.7 FUTIDX contracts (2026-07-08; all monthly expiries since 2026-07-10)** — subscribes in **Quote mode**:

> §36 note (2026-07-08): in Quote mode, derivative OI is NOT inline (inline OI bytes 34-37 exist only in the Full packet) — OI arrives as the separate 12-byte code-5 packet (live-market-feed.md rule 9), which the tick processor currently counts-and-drops.

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
| **L4 PREVENT** | Validate parsed universe size in `[100, 1200]`; HALT if outside. Verify every `UNDERLYING_SECURITY_ID` from FUTSTK/OPTSTK rows resolves to an NSE_EQ row in the same CSV (no dangling references). | Misparsed CSV; runaway subscription cost |
| **L5 AUDIT** | `instrument_fetch_audit` row per attempt (success or failure) + `instrument_lifecycle_audit` row per state transition + `tv_universe_size{kind=...}` CloudWatch metric + tracing span | Continuous observability + forensic chain |
| **L6 RECOVER** | Infinite retry (per §4). NEVER fallback to a different data source. NEVER proceed with stale data. | Operator-locked: no silent degradation |
| **L7 COOLDOWN** | Retry backoff capped at 300s between attempts; never burst Dhan with >5 GETs/min | Self-induced rate limit |

---

## §10. Boot sequence integration

The new `instrument_lifecycle` orchestrator slots between existing Step 6 (auth) and the WebSocket pool spawn:

```
08:30 IST  EventBridge cron fires (Mon–Fri only); EC2 r8g.large boots (~60s cold)
08:31      Docker compose up (QuestDB + tickvault-app)
08:31:30   App Step 1-5: CryptoProvider → Config → Observability → Logging → Notification
08:32      Step 6: Dhan auth (TOTP → JWT) — Valkey dual-instance lock acquired
08:32:30   Step 6a: Dhan static IP boot gate (GET /v2/ip/getIP)
08:33      Step 6b: QuestDB DDL — includes new `instrument_lifecycle` + `instrument_lifecycle_audit` tables
08:33:30   ─── NEW: Step 6c — Daily universe orchestrator ─────────────────────
              1. Read yesterday's active set from `instrument_lifecycle` (cold-path bootstrap)
              2. GET Dhan Detailed CSV with L1-L7 defense layers (§9)
              3. Validate + parse + SHA-256
              4. Extract (a) indices (filter §2) + unique F&O underlyings (group by UNDERLYING_SECURITY_ID) → the 331-SID SUBSCRIPTION set (+ the §36.7 all-months FUTIDX rows, 2026-07-10); AND (b) the applicable F&O CONTRACTS (FUTSTK/OPTSTK for resolved underlyings + FUTIDX/OPTIDX for tracked indices; currency/commodity excluded) → MASTER-only set per §5
              5. Compute delta vs yesterday — emit added / expired / renamed transitions (over the full master set incl. contracts)
              6. UPSERT `instrument_lifecycle` (subscription set + applicable-F&O contracts per §5) + INSERT `instrument_lifecycle_audit`
              7. INSERT `instrument_fetch_audit` outcome row
              8. Build `Arc<DailyUniverse>` for the WS subscription dispatcher — dispatcher reads `subscription_targets` ONLY (331), NEVER the contracts (2-WS lock) — the §36.7 all-months FUTIDX rows are promoted INTO `subscription_targets` at build time (2026-07-10), so the dispatcher contract itself is unchanged
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
| `MAX_DAILY_UNIVERSE_SIZE = 1200` constant | Universe size cap; HALT if exceeded | `crates/common/src/constants.rs` (Sub-PR #2/#7) |
| `crates/storage/tests/instance_type_lock_guard.rs` | Source-scan: pins `t4g.large` in `aws-budget.md` + this file + architecture doc §5 | NEW in Sub-PR #1 |
| `crates/storage/tests/daily_universe_scope_guard.rs` | Source-scan: pins Detailed CSV URL + DEDUP key contract + Quote-mode-for-all + retry-policy unbounded loop + I-P1-11 composite key | NEW in Sub-PR #1 |
| Source-scan retirement of `indices4only_scope_lock_guard.rs` | Old ratchet removed | RETIRED in Sub-PR #1 |
| Per-row CSV schema validation | Reject CSV if >0.1% rows fail mandatory-field check | `crates/core/src/instrument/csv_parser.rs` (Sub-PR #4) |
| Lifecycle reconciler test coverage | Idempotent UPSERT + state-flip + dangling-reference rejection | `crates/storage/tests/instrument_lifecycle_*.rs` (Sub-PR #9) |
| RAM-first hot path (UNCHANGED) | banned-pattern scanner blocks SELECT in indicator/strategy/risk paths | already shipped |
| `INDEX_FUTURES_UNDERLYINGS` const (len 4) + `MAX_MONTHLY_EXPIRIES_PER_UNDERLYING = 6` + never-roll all-months selector | §36/§36.7 FUTIDX scope pinned in code + rule text | `crates/storage/tests/daily_universe_scope_guard.rs::{futidx_scope_pinned_to_4_underlyings_all_monthly_expiries, futidx_scope_rule_file_pins_forbidden_remainder, futidx_scope_never_roll_source_pin, futidx_scope_legacy_gate_still_false}` (§36 2026-07-08; §36.7 2026-07-10) |

---

## §12. What a PR that violates this lock looks like (REJECT)

- Removes the `DailyUniverse` enum variant or adds a parallel variant without a dated operator quote
- Re-introduces `Indices4Only` or `FullUniverse` or `IndicesOnlyAllExpiries` or `IndicesUnderlyingsOnly` variants
- Adds a fallback data source (REST LTP, S3 cache, yesterday's stale CSV) to the instrument-fetch path
- Adds a give-up condition to the fetch retry loop (any code path returning Err without retry)
- Changes the subscription mode for ANY universe SID from Quote to Ticker or Full without a dated operator quote
- Adds a 2nd main-feed connection or any new WS endpoint
- Subscribes the daily universe to derivative contracts **beyond the §36 grant** (OPTIDX/FUTSTK/OPTSTK always; FUTIDX beyond the 4 named underlyings, beyond monthly serials `>= today`, or beyond the `MAX_MONTHLY_EXPIRIES_PER_UNDERLYING` envelope) — otherwise only the UNDERLYING_SECURITY_ID spot rows are subscribed (§36 2026-07-08; §36.7 2026-07-10)
- DELETES rows from `instrument_lifecycle` (lifecycle_state transitions are the ONLY allowed mutation; no DELETE statements)
- Changes `effective_main_feed_pool_size` to anything other than 1
- Modifies instance type from r8g.large without the 4-file update protocol in §7 Mechanical Rule 1

Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this rule file FIRST with a dated quote, only then can the PR land. This prevents accidental scope creep through casual approvals.

---

## §13. Honest 100% claim (mandatory wording per `operator-charter-forever.md` §F)

When any PR / commit message / Telegram body invokes "100% guarantee" for the daily-universe pipeline, it MUST be qualified exactly:

> "100% inside the tested envelope, with ratcheted regression coverage:
> infinite retry on instrument CSV fetch with escalating Telegram (Info→High→Critical at attempts 4/11/21);
> app boot BLOCKS until fresh CSV in hand — never proceeds with stale, partial, or corrupt data;
> ≤20-second tick burst absorbed via 100K-tick rescue ring (constant `TICK_BUFFER_CAPACITY`, ratcheted by `zero_tick_loss_alert_guard.rs`);
> beyond 20s, NDJSON spill → DLQ catches every payload as recoverable text;
> all 21 timeframes RAM-resident for today + yesterday across the current universe (~250–1000 SIDs; ~3.2 GB working set, ~7.8 GB headroom on the r8g.large 16 GiB host);
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

**2026-07-09 update — boot-heartbeat window widened 09:10 → 09:20 IST (market-open seam closure):** as shipped, this contract is the `tv-<env>-boot-heartbeat-missing` alarm (`deploy/aws/terraform/boot-heartbeat-alarm.tf` — `tv_boot_completed` missing, `treat_missing_data=breaching`, 2×60s evaluation) with actions gated by a boot-window Lambda. The 2026-07-09 audit found the window's original 09:10 IST close left a SEAM: the market-hours liveness window (`market-hours-liveness-alarm.tf`) opens only at 09:20 IST and its open-mode Lambda resets its alarms to OK (5-period missing-data evaluation → first possible page ~09:25-09:26), so a process death anywhere in [09:10, 09:20) IST — exactly spanning the 09:15 market open — paged nobody inside the seam and at best ~09:25 (up to ~15 min blind). The boot-window close now fires at 09:20 IST (`cron(50 3 ? * MON-FRI *)`), the exact minute the market-hours window opens, so liveness coverage hands over with no seam: a death at 09:10–~09:17 pages within ~2-3 min via the boot alarm; a death in ~[09:16, 09:20) is the honest residual — the boot alarm may not complete its 2-period evaluation before close (CloudWatch's missing-data evaluation range can pad the naive 2×60s by 1-2 periods), and the market-hours alarm pages at ~09:25-09:26 (worst ~9-10 min, inside the ≤10 min envelope). Deliberately NOT done: widening the market-hours window itself to 09:10 — its gate arms 9 alarms whose signals are invalid pre-09:20 (the SLO tick-freshness pre-open pin + 9-of-15 degraded lookback per `wave-3-d-error-codes.md`), which would re-open the pre-open false-page class the 09:20 gate was built to avoid. Ratchet: `crates/common/tests/aws_alarm_semantics_guard.rs::test_boot_heartbeat_window_hands_over_to_market_hours_window`. Cost delta: 0 new alarms, 0 new metrics, 0 new Lambdas (a cron-schedule change only). **2026-07-10 correction:** the gate now arms **11** alarms — the ws-pool pair (`ws-pool-all-dead` + `ws-failed-connections`, app-alarms.tf) joined 2026-07-10 for the pre-09:00 IST Dhan connect-deferral false-page class; unlike the signal-invalid-pre-09:20 set, their gauges ARE valid from the 09:00 pool connect — they are gated for the pre-09:00 deferral false pages plus the accepted 09:00–09:20 handover residual (covered app-side by the 09:16:30 IST market-open self-test; first CloudWatch page ~09:22). The do-not-widen rationale above therefore reads: 9 signal-invalid-pre-09:20 + 2 deferral-gated-but-valid-from-09:00. **2026-07-11 correction (scoreboard PR-C):** the gate now arms **12** alarms — `groww-exchange-lag-p99-high` (silent-feed-alarms.tf S4, the Groww mirror of the Dhan lag alarm) joined the signal-invalid-pre-09:20 set (its gauge publishes in-session only), making the split 10 signal-invalid-pre-09:20 + 2 deferral-gated.

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

### §28.1 — NARROW LIFT for the `security_id` u32→u64 widening (operator-approved 2026-06-29)

**Authorization:** `.claude/plans/active-plan-groww-security-id-u64.md`
(Status: APPROVED, Date 2026-06-29, "Approved by: Parthiban (operator) —
grounded directive, this session"). That plan widens the shared
`SecurityId` alias and every in-memory SecurityId field from `u32` → `u64`
so a 64-bit Groww `exchange_token` (indices set bit 62) folds through the
SAME aggregator/registry/pipeline instead of being silently dropped by the
`u32::try_from` rejection in the Groww bridge.

**Why this touches the frozen area (unavoidable type cascade, NOT a logic
change):** `ParsedTick.security_id` is now `u64`, and the indicator engine
assigns it straight into `IndicatorSnapshot.security_id`
(`engine.rs` `security_id: tick.security_id`). Keeping the frozen structs at
`u32` would require an `as u32` truncation that silently corrupts the very
64-bit Groww ids the change exists to support — strictly worse than the lift.
So the widening propagates, by the compiler, into exactly four frozen files:
- `crates/trading/src/indicator/engine.rs` — `warmup_from_candles` /
  `warmup_count` parameter type + test helpers
- `crates/trading/src/indicator/types.rs` — `IndicatorSnapshot.security_id`
- `crates/trading/src/indicator/obi.rs` — `ObiSnapshot.security_id` +
  `compute_obi` parameter
- `crates/trading/src/strategy/evaluator.rs` — test-helper signatures
  (+ the in-module test files `indicator/tests.rs`, `strategy/tests.rs`)

**Scope of THIS lift (narrow, mechanical):** ONLY the `security_id`/SecurityId
field-and-parameter TYPE may change `u32` → `u64` in the frozen area. NO
indicator math, NO strategy FSM logic, NO `IndicatorEngine::states` layout
semantics change — the `states: Vec<IndicatorState>` field and every indicator
computation are byte-for-byte identical apart from the id type. The two
hot-path agent CRITICAL findings (C1 warmup gate, C2 `states` flat-Vec I-P1-11)
remain TRACKED and UN-remediated.

**Re-bless:** the `BOUNDARY_FILES` manifest in
`operator_boundary_indicator_strategy_guard.rs` is updated (FNV-1a + byte len +
line count) to the post-widening tree on branch `claude/groww-security-id-u64`.
The guard remains active — any FURTHER edit to the frozen area beyond this
recorded lift fails the build again, requiring its own fresh dated quote.

---

## §29. Same-day warm-resubscribe snapshot — operator authorization 2026-05-29

**Operator quote (2026-05-29, this session):**
> "yes go ahead bro merge all the PRs let em run this atalst bro okay?"
> — given in direct response to the disaster-recovery analysis showing
> that an OOM / crash / Docker restart / QuestDB-volume wipe at 11:30 IST
> should NOT force the full cold rebuild before ticks flow again.

**What this authorizes (and bounds):** a date-keyed subscription-plan
snapshot written to the host disk (`data/instrument-cache/plan-snapshot-YYYY-MM-DD.json`),
SEPARATE from the QuestDB volume, used for INSTANT warm-resubscribe on a
**same-day** boot. This is a narrowly-scoped relaxation of §4's "boot
BLOCKS until a fresh validated CSV is in hand" rule — and ONLY for the
same-day-crash case.

| Boot case | Behaviour | §4 fail-closed still applies? |
|---|---|---|
| FIRST boot of the trading day (no snapshot) | Full cold build, infinite retry, boot BLOCKS until fresh CSV | YES — unchanged |
| SAME-DAY re-boot (snapshot present, date matches today) | Instant plan from snapshot (~1ms) → ticks flow → background reconcile refreshes lifecycle master + snapshot | Relaxed for first-tick; reconcile runs in background |
| Snapshot from a PREVIOUS day | Rejected (date mismatch) → falls through to cold build | YES |
| Snapshot corrupt / unknown role | Rejected (fail-closed) → falls through to cold build | YES |

**Why this is NOT "proceeding with stale data" (the §4 prohibition):** a
same-date snapshot means today's universe was already fetched, validated,
and lifecycle-written by an earlier successful boot. The snapshot is a
cache of THAT computation, not a substitute for a missing fetch. The
background reconcile re-runs the full §4 cold build (infinite retry,
fail-closed) and refreshes both the lifecycle master and the snapshot —
so the audit/lifecycle work is DEFERRED past first-tick, never SKIPPED.

**Honest envelope:** this does NOT detect an intraday universe change; it
is a same-day warm cache keyed on the IST trading date. Correctness of
"is today's universe still valid" remains the background reconcile's job.
If the background reconcile fails, ticks still flow from the warm plan and
the next boot retries — lifecycle for TODAY was already written by the
boot that produced the snapshot.

**Source + ratchets:**
- `crates/core/src/instrument/instrument_snapshot.rs` (module + 12 unit tests)
- `crates/app/src/main.rs::load_daily_universe_plan` (fast-path + slow-path),
  `cold_build_daily_universe` (shared helper), `record_instrument_load_telemetry`
- Path-traversal guard: `instrument_snapshot::is_valid_trading_date` (strict
  `YYYY-MM-DD`, fail-closed) — test `test_is_valid_trading_date_rejects_traversal_and_malformed`
- Fail-closed role parse — test `test_to_universe_fails_closed_on_unknown_role`
- Background-reconcile failure logs `error!` with `code = INSTR-FETCH-01`,
  does NOT halt the live warm plan.

---

# §31 — NIFTY Total Market + index→stocks subscription (operator authorization 2026-06-06)

**Operator decisions (2026-06-06, captured via AskUserQuestion — authoritative):**
1. Add a **33rd index — NIFTY Total Market** — to the tracked set (today 31 NSE
   allowlist + 1 BSE SENSEX = 32).
2. Build an **index → its constituent stocks** mapping for every tracked index.
3. **Subscribe ALL NIFTY Total Market constituent stocks (~750)** to the live feed
   in **Quote mode**, on the **existing single main-feed WebSocket** (NO new WS).
4. The other 32 indices: **map only**. The live SUBSCRIPTION set is the **NTM
   union** (NTM is the broadest basket and already contains their members).
5. **F&O stocks remain separately extractable** — each subscribed stock carries a
   **role tag** (`fno_underlying` and/or `index_constituent`, with the owning
   index list). "F&O only" is an **O(1) filter** over the same data — NOT a second
   download and NOT a second subscription/WebSocket.
6. **`MAX_DAILY_UNIVERSE_SIZE` raised 400 → 1200** to fit ~1,000 live SIDs +
   headroom. **DONE in Sub-PR #2 (2026-06-06):** the §2/§9-L4 envelope is now
   `[100, 1200]`; the seal-ring headroom guard floor moved 20× → 5× (still
   ~7.9× at the 1200 cap, well above the chaos-tested 2× IST-midnight-burst
   adequacy; the 200K `SEAL_BUFFER_CAPACITY` L-C1 design lock is preserved).
7. Constituency source: **niftyindices.com** (rebuild the deleted downloader; reuse
   the existing `INDEX_CONSTITUENCY_*` constants). **Operator-provided exact URL
   (2026-06-06):** the NIFTY Total Market constituents (~750 stocks) live at
   `https://www.niftyindices.com/IndexConstituent/ind_niftytotalmarket_list.csv`
   (i.e. `INDEX_CONSTITUENCY_BASE_URL` + `ind_niftytotalmarket_list.csv`). This is
   the constituent-STOCK list for Sub-PR #3/#4; the NIFTY Total Market INDEX value
   itself (the IDX_I allowlist entry) needs its exact Dhan master `SYMBOL_NAME`,
   resolved from the live Detailed master in Sub-PR #3 (NOT guessed).

**What is UNCHANGED (still locked):**
- Exactly **2 WebSocket connections** (1 main-feed + 1 order-update). ~1,000 SIDs
  fit on the one main-feed conn (Dhan cap 5,000/conn). The 2-WS lock holds.
- **Quote mode** for every SID (§8).
- **`(security_id, exchange_segment)` composite uniqueness** + DEDUP UPSERT KEYS
  (I-P1-11). The NTM-union subscription is deduped against the F&O underlyings.
- `instrument_lifecycle` is still NEVER deleted; the new `role` is an added column,
  applied via `ALTER TABLE ADD COLUMN IF NOT EXISTS`.
- Indicators/strategies boundary (§28) is untouched by this work.

**Honest envelope (mandatory per §13 / operator-charter §F):**
- RAM at ~1,000 SIDs × 21 TFs ≈ ~3.2 GB working set on the r8g.large 16 GiB host
  (~7.8 GB headroom). This is a **MEASURED gate** in the persistence/validation
  sub-PR — NOT promised blind. If the measurement exceeds budget, the scope or the
  resident-TF set is revisited with the operator before go-live.
- "~750 NTM stocks" is the expected count; the EXACT count comes from the live
  niftyindices.com constituent file at build time (no hard-coded hallucinated number).

**Implementation sequencing** lives in `.claude/plans/active-plan.md` (Sub-PR #1 =
this authorization record; Sub-PRs #2–#7 = cap+allowlist, downloader, mapping +
role tagging, subscription wiring, persistence + observability + RAM proof, e2e
validation). Each is a separate serial PR with the 15+7 guarantee matrix and the
adversarial 3-agent review. No code sub-PR may widen the subscription beyond the
NTM union without re-confirming against this section.

---

## §31.1 — Constituent → Dhan `security_id` mapping contract (LOCKED 2026-06-06)

> **Authority for Sub-PR #4.** This is the precise, fail-closed mapping the
> constituency code MUST build to. Operator-confirmed mapping approach 2026-06-06.

**Source files (the join):**

| niftyindices `ind_niftytotalmarket_list.csv` (~750 rows) | Dhan Detailed master (`api-scrip-master-detailed.csv`) |
|---|---|
| `Symbol` (NSE ticker, e.g. `RELIANCE`) | `SYMBOL_NAME` |
| `Series` (e.g. `EQ`) | `SEGMENT` (`E`=Equity), `EXCH_ID` (`NSE`) |
| **`ISIN Code`** (e.g. `INE002A01018`) | **`ISIN`** column (present in the raw CSV; `CsvRow` must add `isin` via `find("ISIN")` — Sub-PR #3) |
| — | `SECURITY_ID` ← the value we resolve |

**Mapping rule (LOCKED):**

1. **PRIMARY key = ISIN.** Globally unique per security, rename-proof and
   series-proof. Match NTM `ISIN Code` → Dhan row `ISIN`, filtered to
   `EXCH_ID == NSE AND SEGMENT == E AND SERIES == EQ` (cash equity). The matched
   row's `SECURITY_ID` is the answer.
2. **SECONDARY / cross-check = `(Symbol, Series=EQ, NSE, Equity)`.** Validate the
   ISIN hit's `SYMBOL_NAME` ≈ normalized NTM `Symbol`; also the fallback if a
   constituent ever lacks an ISIN. Symbol-ALONE is BANNED as the primary key
   (tickers get reused/renamed; Dhan `SYMBOL_NAME` punctuation may differ).
3. **O(1) build:** construct a `HashMap<ISIN, (security_id, ExchangeSegment)>` from
   the Dhan NSE-EQ rows ONCE (cold path), then each of the ~750 constituents is an
   O(1) ISIN lookup. No per-constituent scan of the master.
4. **Fail-closed (NTM membership tolerance = 2%, operator lock 2026-06-08):** a
   constituent whose ISIN resolves to ZERO Dhan NSE-EQ rows is counted + LOGGED
   BY NAME (never silently dropped). If `> 2%` of constituents are unresolved,
   REJECT the build; at or below 2% the unresolved few are skipped and the rest
   subscribed. This NTM membership-list tolerance was raised from 0.5% → 2%
   after the live 2026-06-08 boot degraded the entire NTM universe (244 of ~1000)
   because 5 of 748 stragglers (0.67%) exceeded the old 0.5% bar. It is
   DELIBERATELY looser than — and SEPARATE from — the order-critical Dhan-master
   F&O dangling guard (§3 / §26, line above, unchanged at 0.5%). Pinned by
   `constituent_resolver::tests::test_dangling_reject_fraction_is_two_percent`.
5. **Dedup:** resolved SIDs are unioned with the existing F&O underlyings by
   `(security_id, exchange_segment)` (I-P1-11). No double-subscribe.
6. **Role tagging:** each resolved stock carries `role` ∈ {`index_constituent`,
   `fno_underlying`} (both if applicable) so "F&O only" is an O(1) filter, not a
   second download/WebSocket (per §31 item 5).
7. **NTM INDEX value (separate):** the IDX_I allowlist entry for NIFTY Total Market
   is resolved from the Dhan master `SYMBOL_NAME` in Sub-PR #3 — NOT guessed, NOT
   derived from the constituent CSV.

**Why ISIN-primary is the honest precise key:** symbols are reassigned/renamed over
time and the two CSVs may format them differently; the 12-char ISIN (`INE…`) is the
immutable security identity, so it is the only join key that cannot silently map to
the wrong/old security. The symbol+series cross-check catches the rare bad-ISIN row.

---

# §36 — Index futures (FUTIDX) for exactly 4 underlyings, nearest expiry, BOTH feeds (operator authorization 2026-07-08) — EXPANDED to ALL monthly expiries 2026-07-10 (§36.7)

## §36.0 The verbatim operator demand (preserve exactly, do not paraphrase)

**Quote (2026-07-08):**
> "for both dhan and groww we need to add futures and those also should be subscribed along with this, especially only for nifty banknifty and sensex nifty midcap."

("nifty midcap" = NIFTY MIDCAP SELECT = the MIDCPNIFTY index-futures underlying; the Dhan master's
"NIFTY MIDCAP SELECT" literal canonicalizes to `MIDCPNIFTY` via `INDEX_SYMBOL_ALIASES`.)

## §36.1 The grant — one paragraph

The daily-universe SUBSCRIPTION set additionally carries the index-futures contracts of
exactly FOUR underlyings — since 2026-07-10 (§36.7) ALL monthly expiries `>= today`, of which
the nearest expiry is the first — for NIFTY, BANKNIFTY, MIDCPNIFTY (NSE_FNO, ExchangeSegment 2)
and SENSEX (BSE_FNO, ExchangeSegment 8), selected fresh every morning from the same Detailed CSV
(`SM_EXPIRY_DATE`, never guessed/hardcoded), subscribed in **Quote mode (request code 17)** on
the EXISTING single main-feed connection. Nearest = first expiry >= today; index futures
NEVER roll — on expiry day the expiring contract stays subscribed through the 15:30 close (preserves
`test_index_expiry_never_rolls_via_planner`); the next trading day's build advances
automatically. Selection is a pure function of (CSV, IST trading date) evaluated once at build
time; NO intraday resubscribe ever. The Groww watch set gains the SAME logical contracts
(same underlying + same expiry dates; Groww exchange_tokens, kind=ltp, segment=FNO) on the
EXISTING single Groww connection — see groww-second-feed-scope-2026-06-19.md §36. Both feeds
select via ONE shared pure function; a boot-time comparator compares the expiry SET per
underlying and pages FUTIDX-02 on any comparable-month divergence (§36.7).

## §36.2 What stays FORBIDDEN (unchanged REJECT set)

OPTIDX, FUTSTK, OPTSTK subscriptions (master-only forever, §5 unchanged); any FUTIDX beyond the
4 named underlyings (FINNIFTY, BANKEX, NIFTYNXT50, ... = REJECT); any NON-monthly-serial
instrument; any expiry `< today`; more than `MAX_MONTHLY_EXPIRIES_PER_UNDERLYING` distinct
expiries per underlying (fail-closed, never truncated); any mode other than Quote for these
SIDs; any early/intraday rollover (NO intraday resubscribe); any new WebSocket; any order
placement on these contracts (dry_run + OMS untouched). This file must be edited FIRST with a
fresh dated quote for any of the above.

## §36.3 Prev-close / OI honest note

Quote-mode NSE_FNO/BSE_FNO prev-close = Quote packet bytes 38-41 (Ticket #5525125,
`dhan_locked_facts.rs`; ratcheted by the new NSE_FNO/BSE_FNO Quote routing parser tests). OI
arrives as the separate code-5 packet and is NOT captured today — `ticks.oi = 0` for ALL §36
future SIDs (~12 under §36.7). ALL §36 futures are excluded from the prev-day REST fetch and the 15:31 cross-verify
(Dhan-historical FUTIDX support + expiryCode convention UNVERIFIED-LIVE per annexure rule 8) —
`*_pct_from_prev_day` columns read 0, fail-soft.

## §36.4 Honest envelope (mandatory §13 wording)

100% inside the tested envelope, with ratcheted regression coverage: exactly 4 underlyings
pinned by `INDEX_FUTURES_UNDERLYINGS` (arity ratcheted in code AND rule text); ALL monthly
expiries `>= today` selected by ONE shared pure function (`select_index_future_expiries`),
nearest-expiry never-roll pinned by T-1 / T-0 / T+1 / all-past boundary tests + proptest;
per-(underlying, expiry) fail-closed degrade (flood/ambiguity drops only that month, loud
FUTIDX-01 with the month named); serial envelope `MAX_MONTHLY_EXPIRIES_PER_UNDERLYING = 6`
(a flooded master degrades fail-closed, never truncated); Quote mode per the ratcheted §8
lock; snapshot format 3 forces one deterministic cold build on deploy day; cross-feed parity
compares the expiry SET per underlying — comparable-month divergence pages FUTIDX-02,
far-suffix vendor publication lag is an info-level note + counter.
Bandwidth delta (typical ~12 contracts, vendor-controlled): Dhan ~12 × 50 B Quote × ~4 pkts/s
≈ 2.4 KB/s + ~0.15 KB/s code-5 OI packets (<1.5% of the §8 envelope); Groww ~12 LTP subs
(~779 of the 1000 cap, futures cap-priority); RAM ~12 SIDs × 21 TF cells ≈ 39 MB against
~7.8 GB r8g.large headroom; universe ≈ 343 inside [100, 1200]; no buffer/channel constant
changes; no cost impact (no instance/schedule/storage change — §15 step 5 N/A).
NOT claimed: futures OI capture (`ticks.oi = 0` for ALL future SIDs — code-5 packets still
counted-and-dropped); prev-day pct coverage for futures (role-keyed exclusion,
`*_pct_from_prev_day` read 0, fail-soft); Dhan live Quote cadence/field population for
NSE_FNO and especially BSE_FNO FUTIDX — and for months 2..N on BOTH feeds — UNVERIFIED-LIVE
(first session is the probe; the seeded tick-gap detector logs a never-ticking month within
30s via WS-GAP-06); Groww live FNO subscribe_ltp delivery UNVERIFIED-LIVE; how many monthly
serials each vendor actually lists (typically 3; design takes whatever is listed, bounded
≤6); cross-feed row comparability when vendor masters disagree on month depth (DETECTED via
FUTIDX-02/depth-note, not prevented). FAR-MONTH LIQUIDITY: far serials tick rarely (minutes
of legitimate silence, esp. MIDCPNIFTY/SENSEX) — non-nearest-month SIDs are therefore
EXCLUDED from the SLO tick-freshness silent count and the `tv_tick_gap_instruments_silent`
gauge (INDIA-VIX precedent; alarm threshold 40 and the 0.95 SLO boundary unchanged) while
remaining SEEDED for per-SID WS-GAP-06 black-hole visibility.

## §36.5 Mechanical guards added

`daily_universe_scope_guard.rs::{futidx_scope_pinned_to_4_underlyings_all_monthly_expiries,
futidx_scope_rule_file_pins_forbidden_remainder, futidx_scope_never_roll_source_pin,
futidx_scope_legacy_gate_still_false}`; the selector boundary + proptest suite in
`index_futures.rs` (incl. §36.7: `test_select_index_future_expiries_returns_all_at_or_after_today`,
`test_select_index_future_expiries_keeps_expiring_month_and_later_on_t_zero`,
`test_select_index_future_expiries_drops_expired_month_next_morning`,
`test_select_monthly_serial_flood_degrades_whole_underlying`,
`test_cross_feed_parity_far_suffix_is_depth_only`, `test_cross_feed_parity_hole_is_divergence`);
planner/snapshot/Groww ratchets per `.claude/plans/active-plan-futidx-4.md` +
`.claude/plans/active-plan-futidx-allmonths.md` (2026-07-10).

## §36.6 Auto-driver explanation

> Sir, the juice shop already watches the live price of 4 fruit BASKETS. From today the same
> ONE phone line also hears the price of every "delivery-date basket coupon" (future) the
> market currently sells for each of the 4 baskets — this month's, next month's, the far
> month's; on a coupon's last day we keep listening to THAT coupon until closing time, and
> tomorrow's list simply no longer prints it. Both of our two suppliers (Dhan and Groww) pick
> the coupons with the same rule, and an inspector rings a bell if they ever disagree on a
> coupon both should be selling. No new phone lines. No option coupons. No 5th basket.

## §36.7 — ALL available monthly expiries (operator authorization 2026-07-10)

**Quote (2026-07-10, relayed verbatim via the coordinator session):**
> "instead of only one current month futures contracts just take all the futures of these
> indices — I mean take all available applicable months futures."

The §36 grant EXPANDS from the single nearest-expiry contract to **ALL available monthly
FUTIDX expiries `>= today`** per underlying, for the SAME 4 underlyings, BOTH feeds,
whatever the vendor masters list (no hardcoded month count; envelope bound
`MAX_MONTHLY_EXPIRIES_PER_UNDERLYING = 6` per underlying — beyond it the underlying
degrades fail-closed, `MonthlySerialFlood`). The nearest expiry remains the FIRST element
of each selected set and is still the only month counted by the SLO tick-freshness /
silent-instruments alarm math (non-nearest months are legitimately sparse — see
futidx-4-error-codes.md §3). Contracts NEVER roll: the expiring month streams through its
final session (`>= today` keeps it on T-0) and simply falls out of the next morning's
build — no rollover code exists. Quote mode for every contract. Everything else in
§36.2 stays REJECT (OPTIDX, FUTSTK/OPTSTK master-only forever, 5th underlying, non-Quote
mode, intraday resubscribe, new WS). Selection remains ONE shared pure function evaluated
once per build per feed; cross-feed parity compares the expiry SET per underlying —
nearest-month or in-set divergence pages FUTIDX-02 (High); a far-suffix depth difference
(one vendor publishes far serials earlier) is an info-level note + counter, never a page.
