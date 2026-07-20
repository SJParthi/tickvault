# Daily Universe Scope Expansion — Operator Lock 2026-05-27

> **⚠ DHAN SUBSCRIPTION CONTRACT RETIRED 2026-07-13 (operator directive — see `websocket-connection-scope-lock.md` "2026-07-13 Amendment"):** the Dhan live main-feed WS is retired (operator verbatim Q1: *"now remove this entire Dhan live websocket feed instruments subscription even entire live websocket feed itself..."*; Q3: *"Just Dhan live websocket feed instruments download — I mean the entire process completely related to Dhan live websocket feed itself should be switched off entirely or removed"* + verbatim intent: *"hereafter no Dhan instrument download/parsing — just direct hardcoded security IDs passed to spot 1m and option chain"*). Effects on THIS lock:
> **(a) The daily-universe SUBSCRIPTION contract is RETIRED** — the §1/§2 ~343-SID Quote-mode subscription, the §3 daily Detailed-CSV fetch, the §4 infinite-retry boot-block, the §9 L1–L7 fetch defense, the §10 Step-6c orchestrator, the §20/§22/§29 escape valves, and `SubscriptionScope::DailyUniverse` are all retired; the Phase C code PRs delete the chain (INSTR-FETCH-01..04 / NTM-CONSTITUENCY-01 retire with it). There is NO Dhan instrument download at all — the retained Dhan REST pulls use the hardcoded `SPOT_1M_REST_INDICES` (13/25/51).
> **(b) `instrument_lifecycle` / `instrument_lifecycle_audit` / `index_constituency` SEBI retention STANDS UNCHANGED (§5/§6/§25)** — rows are NEVER deleted; `feed='dhan'` rows simply stop being written (retained as point-in-time history), the Groww `shared_master_writer` keeps writing `feed='groww'` rows, and the process-global ts-pin migration is KEPT.
> **(c) Groww's watch build is UNAFFECTED** — it consumes its OWN master CSV + the niftyindices NTM list (Verified: zero Dhan-CSV consumption in `crates/core/src/feed/groww/`); the §31/§31.1 NTM contract lives on in the Groww resolver (`build_isin_token_map`). *(2026-07-15 note: with the Groww live feed retired, the watch build now runs from the `[groww_universe]` daily rider — `crates/app/src/groww_universe.rs` — not a live lane; it feeds the REST legs' identity resolution + the feed='groww' master, no subscription.)*
> **(d) §36 FUTIDX: the DHAN leg is RETIRED with the subscription contract; the GROWW leg STANDS** (§36.7 all-months, envelope ≤6/underlying, never-roll). The shared selector `index_futures.rs::select_index_future_expiries` becomes single-feed (must be DE-GATED from the `daily_universe_fetcher` cargo feature in Phase C or the Groww futures silently drop — scope violation); the FUTIDX-02 cross-feed parity comparator goes structurally DORMANT (fires only when both feeds record; variant retained — see `futidx-4-error-codes.md`). *(2026-07-15 note: the GROWW leg's LIVE SUBSCRIPTION is retired with the Groww live feed — the futures rows remain in the daily watch FILE for REST contract-identity resolution only.)*
> **(e) The §7 instance/schedule/cost lock (r8g.large, 08:30–16:30 IST, ~₹2,919/mo) is UNTOUCHED by this banner.**
> Sections below are retained as historical audit per house convention; where they conflict with the 2026-07-13 amendment, the amendment wins.

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §I > this file > `websocket-connection-scope-lock.md` (SUPERSEDED below) > `aws-budget.md` (SUPERSEDED below) > defaults.
> **Scope:** PERMANENT. Every Phase. Every PR. Every future Claude/Cowork session.
> **Operator-locked:** 2026-05-27 (verbatim quotes preserved below).
> **Status:** Sub-PR #0 of the 14-PR sequence drafted in the 2026-05-27 planning chat.
> **Supersedes:** the 2026-05-18 t4g.medium lock + 2026-05-15 `Indices4Only` scope lock + the 4-SID `LOCKED_UNIVERSE`. Prior sections retained as historical audit trail in the 4 cross-referenced files.
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The verbatim operator demands (preserve exactly, do not paraphrase)

> **[ARCHIVED 2026-07-20]** §0 Quotes 1–7 (2026-05-27..2026-06-30 — universe expansion, m8g/r8g instance history, all superseded by Quote 8's t4g.medium downsize + the 2026-07-13 subscription retirement) — moved verbatim to `docs/rules-archive/daily-universe-scope-expansion-2026-05-27-archive.md` (context-size incident; content unchanged).
**Quote 8 (2026-07-15, downsize = t4g.medium + QuestDB 1g, automated):**
> "Flip tonight: t4g.medium, QuestDB 1g, automated"
>
> Operator authorized (the quote's exact scope — nothing more): the host instance
> downsizes from **r8g.large** (2 vCPU / 16 GiB) → **t4g.medium** (ARM Graviton2
> burstable, 2 vCPU / **4 GiB**), QuestDB `QDB_MEM_LIMIT` 4g → **1g** (compose
> default + the on-box `deploy/docker/.env` override, retuned via SSM in the same
> run), executed AUTOMATED via a new guarded one-shot `workflow_dispatch` GitHub
> Actions workflow (`.github/workflows/downsize-instance.yml`, reusing the deploy
> workflow's existing AWS credentials), with the old 50 GB root volume snapshotted
> FIRST (rollback artifact, kept ~1 week). Live ap-south-1 on-demand =
> **$0.0224/hr** (the 2026-05-18-verified console rate — re-verify at execution).
> The **Elastic IP is KEPT** (Dhan static-IP + the SSM path). This dated quote
> satisfies §7 Mechanical Rule 1 for the instance change.
>
> **Executor decision (2026-07-15, NOT operator-quoted — recorded separately per
> §15, no scope put in the operator's mouth):** the terraform `ebs_gp3_size_gb`
> default flips 50 → **20 GB** ONLY to pre-stage a later terminate-and-recreate
> in the operator's post-market data-erase window. Shrinking the live 50 GB root
> in place is IMPOSSIBLE (gp3 `modify-volume` grows only, and a 50 GB snapshot
> cannot restore into a 20 GB volume), so TONIGHT'S flip keeps the live 50 GB
> root. Terraform never touches the live volume (`root_block_device[0].volume_size`
> is in `lifecycle.ignore_changes`); the box is fully cattle-provisioned by
> `user-data.sh.tftpl`, so the 20 GB root lands only with a deliberate instance
> replacement, guarded by tonight's snapshot.
>
> *(2026-07-19 correction: the "live 50 GB root" premise of this executor note was
> factually WRONG — the live root was verified **30 GiB** by `describe-volumes` on
> 2026-07-19; the 2026-07-13 approved 30→50 grow never physically applied. See the
> 2026-07-19 approvals bullet below + the §7 dated correction note. The note's
> logic is otherwise unchanged: gp3 still cannot shrink, and 30→20 still requires
> the fresh-volume recreate.)*

**Quote 9 (2026-07-19, 30 GB accepted + t4g.medium as-of-now + hard sub-1K target — preserve EXACTLY, typos included):**
> "just 30 gn enough and onl yt4g medium as of now espeicall yentirkey it hsodul be kless than 1k per month dude oikay?"
>
> Operator ruled: (a) the **30 GB root is formally ACCEPTED** — the 2026-07-13
> approved-but-never-applied 30→50 GB grow is **CANCELLED** (closes the
> 2026-07-19 live-volume-correction flagged follow-up; any future grow needs a
> fresh dated quote); (b) **t4g.medium locked as-of-now** (re-affirms Quote 8 —
> no instance change); (c) **NEW HARD BUDGET TARGET: total AWS bill
> < ₹1,000/month incl GST**. The recorded bills below (~₹1,289/mo at 270 hrs /
> ~₹1,077/mo at ~176 hrs) do NOT meet the target on their own — the itemized,
> evidence-backed lever path (snapshot deletion after 2026-07-22, EIP decision,
> alarm-trim menu, 20 GB recreate; which combinations reach <₹1,000) lives in
> `aws-budget.md` "OPERATOR RULING 2026-07-19". The 20 GB fresh-volume TARGET
> stays a separate, un-quoted executor pre-stage — this ruling accepts 30 GB as
> the live size; going below 30 still needs its own operator go.

**Quote 10 (2026-07-19, same day — EIP release for the no-real-orders period; preserve EXACTLY, typos included):**
> "until or unless we flip the real orders static ip is not needed due okay?"
>
> Operator ruled: the Elastic IP release is **APPROVED for the no-real-orders
> period**, with a strict safety order — **VERIFY outbound-without-EIP FIRST,
> release SECOND**. Live verification (coordinator session, 2026-07-19, live
> describe evidence) returned VERIFIED-UNSAFE-STANDALONE: the subnet
> (`subnet-00c8d06903d1482ea`) does carry `MapPublicIpOnLaunch=true` with a
> real IGW route, but ephemeral-public-IP assignment is a LAUNCH-TIME ENI
> attribute — the live ENI `eni-01fdeec2412f55587` (launched 2026-05-24,
> before the subnet flag) can NEVER mint an ephemeral IP, so a standalone
> release bricks the box (this file's §7 "no public IP after a
> stop/modify/start" claim CONFIRMED live). Execution is therefore BUNDLED
> with the erase-window instance RECREATE (the 20 GB fresh-volume pre-stage —
> a fresh launch inherits the subnet auto-assign and gets an ephemeral IP
> every start). Runbook: `docs/runbooks/eip-release.md`; bill path + second
> ruling record: `aws-budget.md` "OPERATOR RULING 2026-07-19 (SECOND, same
> day)". Re-enable for live trading: new EIP + Dhan setIP re-whitelist
> planned ≥7 days before go-live (Dhan's modify cooldown), with fresh dated
> edits HERE + in `websocket-connection-scope-lock.md` first.

> **[ARCHIVED 2026-07-20]** §0 Approvals 2026-05-27..2026-06-30 (superseded history) — moved verbatim to `docs/rules-archive/daily-universe-scope-expansion-2026-05-27-archive.md` (context-size incident; content unchanged).
- 2026-07-15: Approved instance DOWNSIZE r8g.large → **t4g.medium** (2 vCPU / 4 GiB) + QDB_MEM_LIMIT 4g → 1g, executed via the guarded `downsize-instance.yml` workflow (old root snapshotted first, kept ~1 week), per Quote 8 ("Flip tonight: t4g.medium, QuestDB 1g, automated"). INTERIM bill (the live root stays 50 GB — gp3 cannot shrink) ~₹3,101/mo → **~₹1,471/mo** incl GST at 270 hrs; drops to ~₹1,197/mo only AFTER the 20 GB fresh-volume recreate (executor pre-stage, not operator-quoted); ~₹986/mo requires BOTH the ~176-hr pure auto-schedule basis AND the post-recreate 20 GB volume (on the live 50 GB root the ~176-hr figure is ~₹1,260), NEVER the 270-hr figure. EIP kept.
- 2026-07-19: **LIVE-STATE CORRECTION (verified evidence, not a new approval):** `aws ec2 describe-volumes` on `vol-073ccaa417a0f344b` (the root of `i-0b956d0209231a48b`, attached at `/dev/xvda` since 2026-05-24) returned **30 GiB gp3 (3000 IOPS / 125 MiB/s), in-use** — run live 2026-07-19 via the coordinator session's credentialed AWS access. The 2026-07-13 approved 30→50 GB grow (bullet above) was RECORDED but **never physically applied** — every "live 50 GB root" statement dated 2026-07-15 (the Quote 8 executor note + the bullet above + §7) is corrected by the dated 2026-07-19 notes in §7. **FLAGGED FOLLOW-UP:** the disk-pressure remediation that grow was approved for is therefore UNAPPLIED — the 82%-disk-pressure risk class may recur; applying the grow (or formally accepting 30 GB) is an operator/infra decision, deliberately NOT taken in the docs-only PR carrying this note.
- 2026-07-19: **RULING (Quote 9): 30 GB ACCEPTED + t4g.medium as-of-now + hard target < ₹1,000/mo incl GST.** Resolves the flagged follow-up in the bullet above — the 2026-07-13 30→50 grow is **CANCELLED**; the accepted mitigation for the disk-pressure class is code retention + S3 archival on the 30 GB root (any future grow needs a fresh dated quote). The recorded interim bills (~₹1,289/mo at 270 hrs; ~₹1,077/mo at ~176 hrs) EXCEED the new target — the itemized lever path + which combinations reach <₹1,000 (none without an operator-gated lever) is recorded in `aws-budget.md` "OPERATOR RULING 2026-07-19"; the budget-alarm kill ceiling stepped $55 → $25 the same day with a dated ratchet ladder toward $10 (₹1,000 ÷ 1.18 ÷ ₹85 ≈ $10 pre-GST).
- 2026-07-19: **RULING (Quote 10, later same day): EIP release APPROVED for the no-real-orders period — verify-first, bundled execution.** Live verification (coordinator session, 2026-07-19) proved a standalone release UNSAFE (launch-time ENI attribute — the live ENI never mints an ephemeral IP), so the release lands ONLY inside the erase-window recreate bundle per `docs/runbooks/eip-release.md`; the Lever-2/Lever-5 rows in `aws-budget.md` are updated in lockstep. Post-bundle bill lands in the ~₹600–720/mo class at ~176 hrs (~₹808–929 at the 270-hr ceiling) — under the Quote 9 target at BOTH hour bases.

---

> **[ARCHIVED 2026-07-20]** §1 The rule (retired subscription contract) — moved verbatim to `docs/rules-archive/daily-universe-scope-expansion-2026-05-27-archive.md` (context-size incident; content unchanged).
## §2. The complete allowed set (POST-2026-05-27)

| WebSocket | Count | Endpoint | Allowed instruments | Mode |
|---|---|---|---|---|
| **Main feed** | **1** | `wss://api-feed.dhan.co?version=2&token=<JWT>&clientId=<ID>&authType=2` | Daily-fetched universe (~250 SIDs): all `IDX_I` rows where `EXCH_ID IN (NSE, BSE)` AND `INSTRUMENT == INDEX` (~30) + every unique `UNDERLYING_SECURITY_ID` referenced by `FUTIDX/OPTIDX/FUTSTK/OPTSTK` rows, resolved to its NSE_EQ row (~218) + ALL available monthly FUTIDX expiries of the 4 underlyings (NIFTY/BANKNIFTY/MIDCPNIFTY = NSE_FNO, SENSEX = BSE_FNO; typically ~12 contracts, envelope ≤24) per §36/§36.7 (2026-07-10) | **Quote (request code 17)** — 50-byte packets, gives day OHLC at fixed byte offsets |
| **Order update** | **1** | `wss://api-order-update.dhan.co` | Receives order events for orders WE place; filter `Source=P` | JSON, MsgCode 42 auth |

**Total live WebSocket connections to Dhan: 2** (UNCHANGED from prior lock).

**Universe size envelope (mechanical bound):** `MAX_DAILY_UNIVERSE_SIZE = 1200` (raised from 400 per §31, NTM expansion 2026-06-06). Boot HALTS if computed universe is outside `[100, 1200]`. Fits comfortably on 1 main-feed connection (Dhan cap = 5,000 SIDs/conn). The §36.7 FUTIDX all-months grant (2026-07-10) adds the vendor-listed monthly serials (~12 SIDs typical, ≤24 by envelope) to the subscription set — still trivially inside `[100, 1200]` (≈343 total).

**Subscription dispatch:** 250 SIDs sent in 3 JSON batches (Dhan cap = 100 SIDs/message), sequential with `SubscribeRxGuard` (PR #337) preserving subscription state across reconnects.

---

> **[ARCHIVED 2026-07-20]** §3 Dhan Detailed CSV source + §4 infinite retry policy (retired 2026-07-13) — moved verbatim to `docs/rules-archive/daily-universe-scope-expansion-2026-05-27-archive.md` (context-size incident; content unchanged).
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

## §7. Instance lock — t4g.medium (LOCKED 2026-07-15, supersedes the 2026-06-30 r8g.large + 2026-05-29 m8g.large + 2026-05-27 t4g.large + 2026-05-18 t4g.medium locks)

**2026-07-15 change (operator Quote 8):** instance lock → **t4g.medium**
(ARM Graviton2, burstable general-purpose) — **4 GiB RAM** (DOWN from the
r8g.large 16 GiB), with QuestDB re-capped at `QDB_MEM_LIMIT=1g` in the same
flip. Rationale: the Dhan live WS + its instrument chain retired 2026-07-13
(Groww-only runtime, ~770-SID universe), so the 16 GiB memory-optimized
headroom is no longer earning its premium; t4g.medium is the cheapest
2-vCPU Graviton that fits the §7 Rule 2 budget below. BURSTABLE caveat
(honest): t4g baseline is 20%/vCPU with CPU credits — the old aws-budget.md
analysis blessed it for the 4-SID universe; the ~770-SID + 21-TF +
per-minute-REST workload is NOT yet live-validated on credits — watch
`CPUCreditBalance` after cutover (t4g.large 8 GiB is the rip-cord).

| Spec | Value |
|---|---|
| Instance | **t4g.medium** — ARM Graviton2, **2 vCPU, 4 GiB RAM** (burstable general-purpose) |
| Region | ap-south-1 (Mumbai) |
| Tenancy | Default (Shared) |
| Pricing | On-demand **$0.0224/hr** (ap-south-1, console-verified 2026-05-18 — re-verify at execution) — no Reserved / Savings Plan / Spot |
| Schedule | **Trading weekdays only (Mon–Fri), 08:30–16:30 IST auto** (start `cron(0 3 ? * MON-FRI *)`, stop `cron(0 11 ? * MON-FRI *)`) — narrowed back from 08:00–17:00 on 2026-06-05 per operator ("make the aws instance start and stop from 8.30 am till 4.30 pm"; supersedes the 2026-06-02 widening). Out-of-window runs = operator manual start. Weekends + holidays = OFF unless manually started. |
| EBS | gp3 **30 GB LIVE — ACCEPTED by the 2026-07-19 ruling (Quote 9: "just 30 gn enough"; the 2026-07-13 30→50 grow is CANCELLED)** (VERIFIED 2026-07-19 via live `describe-volumes` — the grow was recorded but never physically applied; this row read "50 GB LIVE" 2026-07-15→2026-07-19. gp3 cannot shrink — the 20 GB target lands only via the fresh-volume terminate-and-recreate in the operator's erase window; terraform default pre-staged to 20, executor decision 2026-07-15) |
| EIP | 1 (24/7) — **KEPT** (`enable_eip = true`, 2026-05-31 flip; without it the box has no public IP after a stop/modify/start → unreachable by SSM + Dhan). **2026-07-19 Quote 10 supersession note: release APPROVED for the no-real-orders period — execution ONLY via the bundled erase-window recreate** (a standalone release is VERIFIED-UNSAFE on the live ENI — launch-time attribute, live describe evidence, coordinator session, 2026-07-19; this row's "no public IP after stop/modify/start" claim CONFIRMED). Runbook: `docs/runbooks/eip-release.md`. Re-enable + Dhan setIP ≥7 days before live orders. |
| Network | ENA enabled by default |

### Cost bill (LOCKED INTERIM ~₹1,289/mo incl. 18% GST — 270 hrs, live 30 GB EBS verified 2026-07-19; drops to ~₹1,197/mo after the 20 GB recreate; was ~₹3,101 on r8g.large pre-2026-07-15)

> **2026-07-19 correction (live-volume verification — arithmetic shown):** the
> root volume is **30 GiB gp3**, not 50 — `aws ec2 describe-volumes
> vol-073ccaa417a0f344b` (run live 2026-07-19 via the coordinator session):
> 30 GiB gp3, 3000 IOPS / 125 MiB/s, in-use, attached to `i-0b956d0209231a48b`
> at `/dev/xvda` since 2026-05-24. The 2026-07-13 approved 30→50 grow was never
> physically applied. Recomputed on this section's own discipline:
> EBS $0.0912 × 30 = **$2.74** (was $4.56 at the recorded 50);
> subtotal $6.05 + $3.60 + $2.74 + $0.18 + $0.28 = **$12.85** (was $14.67)
> → ×₹85 = ₹1,092 → ×1.18 GST = **~₹1,289/mo** (was ~₹1,471/mo on the
> never-applied 50 GB record). The ~176-hr auto-schedule figure on the live
> 30 GB root: $0.0224 × 176 = $3.94; $3.94 + $3.60 + $2.74 + $0.18 + $0.28 =
> $10.74 → ₹913 → ×1.18 ≈ **~₹1,077** (was ~₹1,260 on the 50 GB record).
> The post-recreate figures are UNCHANGED (they already assumed the 20 GB
> volume): ~₹1,197 at 270 hrs, ~₹986 at ~176 hrs. The table below is edited in
> place to the verified 30 GB; the pre-correction 50 GB numbers are preserved
> in this note. **FLAGGED FOLLOW-UP:** the 2026-07-13 disk-pressure grow is
> UNAPPLIED — see the 2026-07-19 approvals bullet in §0.
>
> **Same-day 2026-07-19 ruling annotation (Quote 9):** 30 GB is now ACCEPTED and
> the grow CANCELLED; and BOTH corrected figures carry the new HARD TARGET
> **< ₹1,000/mo incl GST** — neither ~₹1,289 (270 hrs) nor ~₹1,077 (~176 hrs)
> meets it. The itemized lever path (and the plain statement that no
> non-operator-gated combination reaches <₹1,000) lives in `aws-budget.md`
> "OPERATOR RULING 2026-07-19".

Operator-set ceiling **270 running hours/month** (auto weekday schedule
~176 hrs + manual runs — the hours BASIS is unchanged; every prior §7 bill
used it). **EBS = 30 GB live** (verified 2026-07-19 — the 2026-07-13 grow to
50 never applied; shrink impossible in place — Rule 3).
**Elastic IP KEPT**. t4g.medium @ $0.0224/hr, $1 ≈ ₹85. **Every running
component is itemised below — monitoring, alerting, Docker, Lambdas,
Telegram are all included and free-tier.**

| Line | Calc | USD |
|---|---|---|
| EC2 t4g.medium (hosts app + Docker + QuestDB) | $0.0224/hr × 270 hrs | $6.05 |
| Elastic IP (24/7, KEPT) | $0.005/hr × 720 hrs | $3.60 |
| EBS gp3 30 GB (LIVE, verified 2026-07-19 — was recorded 50 / $4.56; the 20 GB post-recreate line is $1.82) | $0.0912 × 30 | $2.74 |
| S3 cold (aged-out partitions) | tiny, grows over time | $0.18 |
| Docker (QuestDB + tickvault containers) | runs on the EC2 host | $0.00 |
| CloudWatch metrics / alarms / Logs / Dashboards | BASE free tier only — the ~$2.7/mo of dated COST-NOTE alarms in aws-budget.md is NOT itemised here (see the honest envelope below) | $0.00 |
| Lambda (telegram-webhook, budget-killswitch, triage) | free tier = 1M req/mo | $0.00 |
| SNS → Telegram + Email fan-out | free tier (1M / 1k) | $0.00 |
| SNS → SMS (optional) | ~100 India msgs | $0.28 |
| Data transfer out | ~14 GB < 100 GB free egress | $0.00 |
| **Subtotal (pre-GST)** | | **$12.85** |
| **× ₹85/$** | | **₹1,092** |
| **+ 18% GST (AWS India)** | | **~₹1,289/mo** *(target < ₹1,000 per the 2026-07-19 ruling — NOT met by this bill; lever path in `aws-budget.md`)* |

**Honest envelope:** the CURRENT bill is ~**₹1,289/month all-in incl. GST**
(270 hrs, live 30 GB root verified 2026-07-19, EIP kept) — a ~₹1,810/mo cut
from the r8g.large ~₹3,101 (the 2026-07-15→2026-07-19 record stated ~₹1,471
on the never-applied 50 GB assumption — see the dated correction note above).
**~₹1,197/mo applies ONLY after the 20 GB fresh-volume recreate**
(subtotal $11.93 → ₹1,014 → ×1.18 ≈ ₹1,197; the EBS line moves $2.74 →
$1.82). The operator's earlier ~₹986/mo figure **requires BOTH the ~176-hr
pure Mon–Fri auto-schedule basis AND the post-recreate 20 GB volume**
($9.82 → ₹835 → ×1.18 ≈ ₹986); on the LIVE 30 GB root the ~176-hr figure
is **~₹1,077** ($10.74 → ₹913 → ×1.18 ≈ ₹1,077; the superseded 50 GB record
put it at ~₹1,260 — $12.56 → ₹1,068 → ×1.18 ≈ ₹1,260; *2026-07-19 ruling
annotation: target < ₹1,000 per Quote 9 — even this ~176-hr figure does NOT
meet it; lever path in `aws-budget.md`*) — ~₹986 is NEVER to be
presented as the 270-hr figure or as achievable before the recreate, and
the hours basis is NOT re-based by this change. **The observability stack
is NOT ₹0** (corrected 2026-07-15 — an earlier draft of this section said
"costs ₹0"): the BASE CloudWatch/Lambda/Telegram+Email fan-out design sits
in the free tier per §7 Rule 5, but the dated COST NOTES accumulated in
`aws-budget.md` (silent-feed +$1.50, REST-audit +$0.60, order-side +$0.60,
scoreboard +$0.40, PR-C3 −$0.40) total ~**$2.7/mo ≈ ₹271/mo incl GST** of
live alarm/metric spend that the bill table above does NOT itemise (the
same omission every prior §7 headline bill carried); optional SMS is ~₹24
on top. The EIP is kept because an
`aws ec2 modify-instance-attribute` instance-type flip (stop→modify→start)
leaves the ENI with NO ephemeral public IP (auto-assign-public-IP is a
fresh-launch-only attribute), so only the EIP gives the box an internet
path to SSM + Dhan *(mechanism CONFIRMED live 2026-07-19; per Quote 10 the
EIP is release-APPROVED for the no-real-orders period via the bundled
erase-window recreate ONLY — see the §7 EIP row note +
`docs/runbooks/eip-release.md`)*. **Tax:** 18% GST total (IGST inter-state, or CGST 9% +
SGST 9% intra-state — identical 18%, no extra cess). Verified: t4g.medium
$0.0224/hr (ap-south-1 console 2026-05-18 — re-verify at execution);
EIP/EBS/S3/SNS are AWS list rates. Budget alarm ceiling stays $35/mo
pre-GST (lowering toward ~$15 is an optional follow-up with its own cost
note in aws-budget.md). Operator approved 2026-07-15 (Quote 8).
*(2026-07-19 correction + ruling: the LIVE terraform kill ceiling was
actually $55 since 2026-06-30 — `budget.tf limit_amount`, not the $35
this sentence recorded; per the Quote 9 sub-1K ruling it stepped
$55 → $25 on 2026-07-19, with a dated ratchet ladder toward $10
(₹1,000 ÷ 1.18 ÷ ₹85 ≈ $10 pre-GST) recorded in `aws-budget.md`.)*

> **Note on instance schedule (2026-05-29):** trading WEEKDAYS only
> (Mon–Fri), **08:30–16:30 IST** auto start/stop. Weekends + NSE holidays
> = instance OFF unless the operator manually starts it. The 08:30 start
> gives the cold-boot + Step 1–6 auth + 08:45 CSV-fetch retry budget so
> the app is ready before 09:00 market open; 16:30 stop is ~1h after the
> 15:30 close (covers post-close digest + flush). Earlier plans
> (08:00–17:00 7-day, then 08:30–17:00 7-day) are superseded.

### Mechanical Rules (replaces aws-budget.md mechanical rules 1+6)

1. **Instance type is t4g.medium. PERIOD.** Changing it (back to r8g.large, to
   t4g.large, etc.) requires:
   - Operator explicit approval with dated quote (see §0 Quote 8)
   - Update to this file
   - Update to `aws-indices-only-locked-architecture.md` §5
   - Update to `aws-budget.md` (existing file marked SUPERSEDED)
   - Ratchet test `crates/storage/tests/instance_type_lock_guard.rs` updated to pin the new type
   - Update to `deploy/aws/terraform/variables.tf` `instance_type` default + validation
   - Update to `scripts/aws-upgrade-instance.sh` `FROM_TYPE` default

2. **Host memory budget for t4g.medium (4 GiB total) — Groww-only runtime (~770-SID universe, 21 TFs):**
   - QuestDB process: ~1.0 GB (`QDB_MEM_LIMIT=1g` — compose default + the on-box `deploy/docker/.env`, retuned by the downsize workflow's SSM step)
   - Tickvault app: ~700 MB actual **(the 2026-05-18 4-SID-universe measurement — NOT a ~770-SID measurement)** / 1.5 GB cap (see the FLAG below for what the retained sizing formula predicts at ~770 SIDs)
   - App: seal ring (200K seal cap, fixed): ~29 MB *(2026-07-18: replaces the deleted tick rescue-ring row — the 100K tick ring + its constant died with the dead tick writer, stage-2/4 sweeps; 200_000 seals × 144 B per `seal_ring.rs`)*
   - App: QuestDB ILP write buffer: 25 MB
   - App: 15+ audit-table buffers: 30 MB
   - Tracing / errors.jsonl rotation buffer: 100 MB
   - OS + FS cache + kernel TCP buffers: ~400 MB
   - **Total used: ~2.3 GB (app at the ~700 MB 4-SID actual) – ~3.1 GB (app at the 1.5 GB cap)** — the rows above sum to ~2.27 GB / ~3.07 GB (arithmetic corrected 2026-07-15; an earlier draft said "~2.6–3.1") *(2026-07-18: the seal-ring row replaced the 10 MB tick-ring row, +~19 MB — inside the ~ rounding, totals unchanged)*
   - **Headroom: ~0.9–1.7 GB** — above the 1 GB Linux kswapd floor only while the app stays at/under its cap. **FLAG (honest, unresolved — Assumed until measured; Rule 11, no false-OK):** the pre-downsize Rule 2 sizing formula this file has always carried (≈3.2 MB × SID for the 21-TF today+yesterday RAM-resident set) predicts an app working set of **~2.5 GB at ~770 SIDs** — with QuestDB at 1g that totals ~4.1 GB (2.5 app + 1.0 QDB + ~0.17 buffers + ~0.4 OS) and does **NOT fit in 4 GiB**. The ~700 MB "actual" and the formula cannot both hold at ~770 SIDs; **the first live session on t4g.medium is the measured gate** — read `tv_process_rss_bytes` / RESOURCE-02 and `mem_used_percent` before AND after cutover; if live RSS is materially above ~1.5 GB, 4 GiB does not fit and t4g.large (8 GiB) is the rip-cord. QuestDB at 1g serving today's ~770-SID Groww write load is likewise re-validated live (the old 1g-class budget served the 4-SID universe). BURSTABLE CPU: watch `CPUCreditBalance` after cutover.

3. **EBS = 30 GB gp3 LIVE (verified 2026-07-19 — the 2026-07-13 approved 30→50 grow was recorded but never physically applied); 20 GB is the pre-staged fresh-volume TARGET** (executor decision 2026-07-15, recorded in §0 under Quote 8 — NOT operator-quoted scope). gp3 grows online but can NEVER shrink: `modify-volume` refuses a smaller size and a larger snapshot cannot restore into a smaller volume (the 30 GB snapshot cannot restore into 20 GB), so 30 → 20 requires a volume/instance REPLACEMENT (terraform terminate-and-recreate in the operator's post-market erase window; the box is fully cattle-provisioned by `user-data.sh.tftpl`; the 2026-07-15 pre-downsize snapshot is the rollback, kept ~1 week; the GitHub secret `EC2_INSTANCE_ID` must be rotated to the new id at recreate time). Terraform `ebs_gp3_size_gb` default = 20 documents FRESH-PROVISION intent only — `root_block_device[0].volume_size` is in the instance `lifecycle.ignore_changes`, so `terraform apply` never touches the live volume. History: 10 GB → 30 GB (2026-05-29 Quote 6) → [50 GB approved 2026-07-13 (disk-pressure grow) — RECORDED but never physically applied; live verified 30 GB by `describe-volumes` 2026-07-19] → 20 GB target (2026-07-15) → **30 GB ACCEPTED (2026-07-19 Quote 9 — the 50 GB grow CANCELLED)**. **FLAGGED FOLLOW-UP (2026-07-19):** the unapplied grow means the 2026-07-13 82%-disk-pressure remediation never landed — applying it (or accepting 30 GB) is an operator/infra decision. *(RESOLVED same day by Quote 9: 30 GB is formally ACCEPTED, the grow is CANCELLED — the disk-pressure class is handled by code retention + S3 archival on the 30 GB root; any future grow needs a fresh dated quote. The 20 GB fresh-volume TARGET stays a separate un-quoted executor pre-stage — going below the accepted 30 needs its own operator go.)* The partition manager keeps auto-archiving partitions >90d to the S3 cold bucket (~4× cheaper per GB than EBS), so EBS holds only the hot window.

4. **No paid AWS services** (RDS, ElastiCache, NAT Gateway, ALB) without budget review.

5. **CloudWatch is the sole observability layer** — within free tier (10 metrics + 10 alarms + 5 GB logs).

6. **RAM-first hot path (mandatory, unchanged):** every indicator + strategy + risk decision reads from RAM. QuestDB is persistence + audit + cold-path boot rehydration only. Banned-pattern scanner enforces.

7. **Instance flip tooling:** the 2026-07-15 downsize executes via the guarded one-shot GitHub Actions workflow `.github/workflows/downsize-instance.yml` (snapshot-first → stop → `aws ec2 modify-instance-attribute` → start → EIP identity check → SSM `QDB_MEM_LIMIT=1g` retune → verify; a capacity start-failure rolls back to r8g.large with a VERIFIED post-rollback type/state check; a run that finds the box ALREADY t4g.medium continues in retune-only mode instead of refusing). `scripts/aws-upgrade-instance.sh` remains the manual fallback (`--from r8g.large --to t4g.medium` defaults; a t4g.medium target auto-defaults `QDB_MEM_LIMIT=1g`, an r8g.large target keeps the 4g arm for the emergency roll-UP direction). EIP + EBS preserved on either path (both verify the EIP survives and abort loudly if it changed) — the EIP is mandatory because the stop/modify/start leaves the ENI with no ephemeral public IP. Downtime ~3 minutes; the market-hours guards refuse the in-session window without an explicit force.

---

> **[ARCHIVED 2026-07-20]** §8 Quote-mode subscription, §9 Z+ fetch defense, §10 boot sequence, §11 mechanical guards, §12 REJECT list, §13 honest claim, §14 auto-driver (retired subscription chain) — moved verbatim to `docs/rules-archive/daily-universe-scope-expansion-2026-05-27-archive.md` (context-size incident; content unchanged).
> Still-binding §12 REJECT row (retained for the FUTIDX ratchet): subscribing derivative contracts **beyond the §36 grant** (OPTIDX/FUTSTK/OPTSTK always; FUTIDX beyond the 4 named underlyings or the monthly-serial envelope) = REJECT.
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

> **[ARCHIVED 2026-07-20]** Sub-PR #1.5 enrichment preamble (historical) — moved verbatim to `docs/rules-archive/daily-universe-scope-expansion-2026-05-27-archive.md` (context-size incident; content unchanged).
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

> **[ARCHIVED 2026-07-20]** §20 operator escape valve, §21 sub-PR ordering gate, §22 holiday handling, §23 split/rename classification, §24 audit-chain ordering (retired fetch chain / shipped history) — moved verbatim to `docs/rules-archive/daily-universe-scope-expansion-2026-05-27-archive.md` (context-size incident; content unchanged).
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

> **[ARCHIVED 2026-07-20]** §26 CSV parser robustness + §27 dry-run isolation (retired fetch chain) — moved verbatim to `docs/rules-archive/daily-universe-scope-expansion-2026-05-27-archive.md` (context-size incident; content unchanged).
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

> **[ARCHIVED 2026-07-20]** §29 warm-resubscribe snapshot + §31 NTM subscription authorization (retired subscription chain; §31.1 mapping contract kept live) — moved verbatim to `docs/rules-archive/daily-universe-scope-expansion-2026-05-27-archive.md` (context-size incident; content unchanged).
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
the t4g.medium 4 GiB host's ~0.9–1.7 GB budgeted headroom (Assumed until live-measured — host
downsized 2026-07-15 per §7 Quote 8; was ~7.8 GB on r8g.large when this envelope was
written); universe ≈ 343 inside [100, 1200]; no buffer/channel constant
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
planner/snapshot/Groww ratchets per `.claude/plans/archive/2026-07-08-futidx-4.md` +
`.claude/plans/archive/2026-07-10-futidx-allmonths.md` (2026-07-10; both archived from
active 2026-07-13 per plan-enforcement rule 7).

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
