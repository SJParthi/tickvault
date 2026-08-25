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
- 2026-07-31: **RULING (Quote 11): budget KILL-CEILING raised $25 → $35 — the sub-₹1,000 TARGET is UNCHANGED, the downward ratchet ladder is PAUSED (not cancelled).** Operator verbatim (typed directly in-session, typos preserved): *"Raise the limit bro olay?"* / *"Go ahead with recomemdnaetion bro okay?"* / *"nommanaul inptu full yauotmated mtoehrfucekr okay?"*. Incident (live AWS evidence, 2026-07-31 ~08:00 IST, acct 208384284948): `tv-prod-monthly-budget-v2` ACTUAL **$27.47 vs LIMIT $25.00 = 109.9% BREACHED**, FORECAST $28.72, and **BOTH** AUTOMATIC budget actions (90% + 100%, `STOP_EC2_INSTANCES` on `i-0b956d0209231a48b`) sitting in **`EXECUTION_FAILURE`** — repeatedly trying to auto-stop the prod box mid-session while failing to complete. $30 was REJECTED because its 90% line ($27.00) already sits below the live actual; $35 puts the 90% line at $31.50 and the forecast at 82%. EventBridge verified all 17 rules ENABLED (start `cron(0 3 ? * MON-FRI *)` = 08:30 IST, stop `cron(0 11 ? * MON-FRI *)` = 16:30 IST — the operator's requested window, already live), so the ceiling was the sole blocker. 3-way lockstep applied (`budget.tf` + `budget-guards.tf` + `budget_digest.rs`). Standing waste of ~$8.31/mo (Cost-Explorer polling $2.38, EIP $3.55, CloudWatch $3.27, Secrets Manager $0.38) is UNADDRESSED by the raise and is the precondition for resuming the ladder — full record in `aws-budget.md` "OPERATOR RULING 2026-07-31".

**Quote 12 (2026-08-07, instance type change to escape an AWS capacity outage — preserve EXACTLY):**
> "go ahead with this Different instance type"

Given in direct response to a presented choice between (a) a multi-AZ rebuild at
₹0/month extra and (b) a different instance type at roughly +₹400/month. The
operator picked (b).

**The incident (Verified, live AWS evidence, 2026-08-07):** the prod box failed
to start at the 08:30 IST schedule with `InsufficientInstanceCapacity` — AWS had
no **t4g.medium** capacity in **ap-south-1a**, and `main.tf:77` pins the instance
to that single AZ (`availability_zone = "${var.aws_region}a"`), so a stopped
instance can only restart there. Six start attempts between 08:58 and 10:03 IST
all failed. CloudWatch CPU shows the same failure across the week: Aug 3 = 8h,
Aug 4 = 5h, **Aug 5 = 0h (never ran)**, Aug 6 = 7h, **Aug 7 = 0h**. Budget was
NOT involved ($2.87 actual vs the $35 ceiling).

**The change:** instance type **t4g.medium → t4g.large** (ARM Graviton2
burstable, 2 vCPU / **8 GiB**, ap-south-1 on-demand **$0.0448/hr** — 2× the
t4g.medium rate; re-verify at execution). Same family, same AZ, same EBS, EIP
preserved — a different instance type draws from a DIFFERENT capacity pool,
which is the entire point of the change.

**Honest cost (this quote's real consequence — recorded, not buried):** the
Quote 9 hard target of **< ₹1,000/mo incl GST is NOT met and moves further
away**. Recomputed on this section's own discipline, on the live 30 GB root
with the EIP kept: at the 270-hr ceiling $0.0448 × 270 = $12.10; $12.10 + $3.60
+ $2.74 + $0.18 + $0.28 = **$18.90** → ₹1,607 → ×1.18 ≈ **~₹1,896/mo** (was
~₹1,289). At the ~176-hr pure auto-schedule basis: $7.88 + $3.60 + $2.74 + $0.18
+ $0.28 = **$14.68** → ₹1,248 → ×1.18 ≈ **~₹1,473/mo** (was ~₹1,077). So this
is roughly **+₹400–600/mo**. The Quote 9 target stands as a target; this change
knowingly breaches it to buy availability. The zero-cost alternative — moving to
multi-AZ so the box can start in 1b/1c on the SAME t4g.medium — was presented
and NOT chosen; it remains available and would allow reverting to t4g.medium
later under its own dated quote.

> **⚠ EXECUTION OUTCOME 2026-08-07 — THE FLIP FAILED; THE BOX IS STILL t4g.medium.**
> The authorized change was executed via `downsize-instance.yml` (run 31148235540)
> at ~10:12 IST and **AWS refused t4g.large for the SAME reason**:
> `InsufficientInstanceCapacity ... when calling the StartInstances operation`.
> The workflow rolled back to t4g.medium, VERIFIED the rollback, and re-stopped
> the box for schedule parity; rollback snapshot `snap-0573ab07252f67bf3` was
> taken first and nothing irreversible happened. **Live state: t4g.medium,
> stopped.**
>
> **What this PROVES (and it supersedes the reasoning above):** the constraint is
> the **AVAILABILITY ZONE, not the instance type**. `ap-south-1a` is out of
> capacity for t4g.medium AND t4g.large simultaneously, so no instance-type
> change can fix this — the ₹400–600/mo would have bought nothing. The remaining
> real fix is the ZERO-extra-cost one: **un-pin the single AZ** (`main.tf:77`
> hardcodes `availability_zone = "${var.aws_region}a"`) so the box can launch in
> `1b`/`1c`, which requires an instance REPLACEMENT (AZ is fixed at launch) and
> therefore a new EIP — see the §7 EIP row and `docs/runbooks/eip-release.md`.
>
> This lock's TYPE remains **t4g.large as the authorized target** (Quote 12
> stands, and 8 GiB still retires the Rule 2 FLAG), but it is **NOT APPLIED**.
> Any future re-attempt should expect the same capacity refusal until the AZ pin
> is addressed. Re-attempting the type flip alone, without the AZ fix, is
> predicted to fail again — do not burn a session on it.

**Bonus (not the reason, but real):** 8 GiB retires the §7 Rule 2 FLAG, which
honestly recorded that the retained sizing formula predicts a ~2.5 GB app
working set at ~770 SIDs — a figure that does NOT fit 4 GiB. That risk was
outstanding and unmeasured; t4g.large removes it.

**Quote 13 (2026-08-08, r8g.xlarge + AZ un-pin + 100 GB + budget ceiling raise — preserve EXACTLY, typos included):**
> "then can we go ahead with r8g x large dude"
>
> "yes dude go ahead but before that provide em tje rpecise detaield lsited plan dude okay?"
>
> "yes raise the ceilign ddue and make it as entirely acceptabel to oru newer requeirmeent dude okau?"

Given in direct response to a presented plan naming exactly these changes: instance
**t4g.large → r8g.xlarge**, the **availability-zone pin removed**, EBS fresh-provision
**20 → 100 GB**, weekday **08:00–17:00** (~210 hrs), and the budget kill-ceiling
**$35 → $100**. The third quote authorizes the ceiling raise and the alignment of the
cost envelope to the new requirement.

**The requirement this serves (operator, 2026-08-08, verbatim):** *"current day ticks
secodns multiple seocdns tiemframes liek 1 seocnd 5 seconds 10 15 30 seocnds dude nad
then even mintue level tiemframes liek 1,2,3,5,15,30,60 and 1 dya also"* — i.e. **13
timeframes** (S1, S5, S10, S15, S30 · M1, M2, M3, M5, M15, M30, M60 · D1) plus **raw
tick retention**, current day, at a target scale of ~25,000 instruments.

**The incident this closes (Verified, live AWS evidence, 2026-08-07/08).** The
2026-08-07 Quote 12 t4g.large flip was itself REFUSED by AWS with
`InsufficientInstanceCapacity` (workflow run 31148235540) and rolled back — proving the
constraint is the **availability zone, not the instance type**. `main.tf:77` pins the
only subnet to `${var.aws_region}a`. CPU evidence: Aug 5 = 0h, Aug 7 = 0h, Aug 8 = 0h.
`describe-instance-type-offerings` (run live 2026-08-08) confirms **every** candidate
type — t4g.medium, t4g.large, r8g.large, r8g.xlarge, r8gd.large, m8g.large, r8i.large —
is offered in **all three** ap-south-1 AZs. The AZ pin is therefore the sole blocker,
and it is removed here: subnets are provisioned in 1a/1b/1c and the instance's zone
becomes `var.availability_zone`, so a future capacity refusal is a one-variable change.

**Why r8g.xlarge specifically.** `r` = 8 GiB RAM per vCPU (memory-optimised); the
workload is memory-bound, not CPU-bound. Sizing (Verified against source): 13 TF ×
`LiveCandleState` **128 B** (`live_candle_state.rs` — 11×f64 + 2×u64 + i64 + 3×u32 =
124, padded) × 25,000 = **42 MB**; seal ring 200,000 × ≤144 B = **29 MB**; one day of
ticks in RAM 2.3–7.2 GB; QuestDB 8–16 GB; app + OS 4–8 GB ⇒ **14–31 GB**, fits 32 GiB.
REJECTED alternatives: `m8g` (4 GiB/vCPU forces buying unused CPU to reach 16 GiB);
`r8gd` (local NVMe is **wiped on stop** and the box stops daily); `r8i` (Intel would
force an x86 rebuild of the entire ARM pipeline including the lambdas).

**Honest cost (recorded, not buried).** r8g.xlarge · 100 GB · ~210 hrs · ticks retained
= **$58.06–$73.60/mo → ~₹5,824–₹7,382/mo incl GST** (EC2 rate DERIVED from the recorded
r8g.large bill: ₹3,101 ÷ 1.18 ÷ 85 = $30.92, minus EIP $3.60 + EBS(50) $4.56 + S3 $0.18
+ SMS $0.28 = $22.30 ÷ 270 hrs = **$0.083/hr** for r8g.large ⇒ **$0.166/hr** xlarge; the
method reproduces the t4g.medium ₹1,289 figure exactly. AWS list may reach ~$0.24/hr —
re-verify on the first invoice). **The Quote 9 sub-₹1,000/month target is BREACHED by
~6×** and is NOT met; this quote knowingly accepts that to serve the 13-timeframe /
25,000-instrument / tick-retention requirement. The Quote 9 target stands as a target
and the downward ratchet ladder remains PAUSED.

**Budget ceiling $35 → $100 (the third quote).** The plan's high estimate is $73.60;
the budget's AUTOMATIC `STOP_EC2_INSTANCES` actions fire at 90% and 100%, so a $35 (or
even $80) ceiling would try to stop the box mid-session. $100 puts the 90% line at $90,
comfortably above the $73.60 worst case, while remaining a real guard rather than a
rubber stamp. **UNRESOLVED, flagged (Rule 11, no false-OK):** the 2026-07-31 ruling
recorded BOTH budget actions stuck in `EXECUTION_FAILURE`; the executing session's
credentials could NOT read `describe-budget-actions-for-budget` (AccessDenied), so
whether they still fail is **Unknown**. Raising a ceiling does not repair a broken
action — the kill-switch may not actually fire. This needs its own live check with
budget-action read access before the safety net can be claimed to work.

**Disk 20 → 100 GB fresh-provision.** Live root is 30 GB (Cost Explorer
`VolumeUsage.gp3` qty=30.0 at $2.736/mo — nothing is free-tier). Estimated load: ticks
25–80 M rows/day (**Assumed**, swings disk 3×) ≈ 44–141 GB/mo; 13 TFs sparse ≈ 46 M
rows/day ≈ 61 GB/mo. Sparse is Verified by `live_candle_state.rs:105` — an unopened
bucket is a sentinel and emits nothing (dense would be 808 M rows/day = 35,900/sec
against the ~5,000/sec envelope; sparse is ~2,050/sec). 100 GB is chosen deliberately
over 250: **gp3 grows online in one command and can NEVER shrink** (Rule 3), so the
small side is the reversible direction. `variables.tf` already permits 10–200 GB.

**EIP: KEPT (no change).** Quote 10 (2026-07-19) approves release for the
no-live-orders period and a bundled recreate is the sanctioned path, but the operator
did not answer the release question in this exchange, so the **reversible default
applies** — the address is retained (~₹360/mo). Releasing remains available under
Quote 10 via `docs/runbooks/eip-release.md`.

**SCOPE HONESTY — capacity is not data.** This quote sizes a box that CAN hold the
requirement. It does NOT deliver it. The runtime is REST-only on the hardcoded
`SPOT_1M_REST_INDICES` (+ INDIA VIX); **no live tick feed exists** (Dhan live WS retired
2026-07-13, Groww live WS retired 2026-07-15, GDF and TrueData are both DEFAULT-OFF
trial-first locks whose implementation PRs never started). Reaching ~25,000 instruments
requires a feed trial plus its own dated scope edits to
`websocket-connection-scope-lock.md` and this file — deliberately NOT authorized here.

**Quote 15 (2026-08-12, r8g.xlarge FINALISED — preserve EXACTLY, typos included):**
> "dude this is our finalsied instancue dude okay? just sue this evrywhere neitlrey dude okay?Instance type — terraform says r8g.xlarge (32 GiB),"

Given in direct response to an audit line reporting the instance type as an
UNCONFIRMED blocker — *"terraform says `r8g.xlarge` (32 GiB), live box was
`t4g.medium` (4 GiB) after the capacity-refused flip rolled back"*. The
operator resolves that ambiguity: **r8g.xlarge is FINAL and is the value every
surface must carry.** This does not change the Quote 13 sizing decision; it
CONFIRMS it and orders the remaining drift closed.

**What this quote settles (the drift it was given to close).** Quote 13 pinned
r8g.xlarge in `variables.tf` (default + validation) and in §7 above, but four
surfaces were left behind — and one of them was live-dangerous:

| Surface | Was | Now |
|---|---|---|
| `.github/workflows/downsize-instance.yml` `TO_TYPE` | **`t4g.large`** — a manually-dispatchable workflow that would have moved the box OFF the locked type | `r8g.xlarge` |
| `scripts/aws-upgrade-instance.sh` `--to` default | `t4g.medium` | `r8g.xlarge` |
| `aws-budget.md` H1 | "t4g.medium LOCKED ~₹1,022/mo" | r8g.xlarge, with the real bill |
| `variables.tf` AMI description | "t4g.medium is Graviton" | r8g.xlarge is Graviton4 |

The `downsize-instance.yml` row is the one that mattered: the lock lived in
terraform's validation, which only binds `terraform apply`. That workflow
mutates the instance through `ec2 modify-instance-attribute` directly, so it
never consulted the validation at all — a lock the enforcement path could walk
straight past.

**⚠ WHAT THIS QUOTE DOES NOT DO — the live box (Rule 11, no false-OK).**
Recording r8g.xlarge everywhere in the repository does NOT make the running
instance r8g.xlarge. The only recorded flip attempt (2026-08-07, Quote 12,
t4g.large) was REFUSED by AWS with `InsufficientInstanceCapacity` and rolled
back; no successful r8g.xlarge apply is recorded anywhere in this repo, and
the executing session has no AWS credentials to check. **The live type is
therefore Unknown from here and must be verified on the box**

> ## ✅ RESOLVED 2026-08-12 — VERIFIED LIVE, and the "no AWS credentials"
> ## premise above was WRONG
>
> The credentials were present the whole time; the `aws` CLI **binary** was
> not installed. Running `scripts/ensure-aws-cli.sh` (the repo's own official
> installer, added in the 2026-08-01 interpreter purge) made every check below
> possible in seconds. Recorded because the error is worth more than the
> result: **an unverified assumption of no-access got written into a rule file
> as a fact**, and it would have kept propagating as "Unknown" until someone
> tried the thing instead of asserting it.
>
> **Live state, `describe-instances` / `describe-volumes` / `describe-addresses`,
> account 208384284948, 2026-08-12:**
>
> | Fact | Live value |
> |---|---|
> | Instance type | **`r8g.xlarge`** — matches the lock |
> | State | `running` |
> | Availability zone | **`ap-south-1b`** — NOT the old 1a pin |
> | Instance id | **`i-0c3fe906dad5492fc`** — NEW; the old `i-0b956d0209231a48b` is gone |
> | Subnet | `subnet-077459dce52a3cd46` — new; not the old `subnet-00c8d06903d1482ea` |
> | Root volume | **100 GB gp3**, in-use |
> | Elastic IP | `13.234.145.177`, still allocated and ASSOCIATED to the new instance |
> | Launch time | `2026-08-12T03:00:38Z` = 08:30 IST — the normal daily start |
>
> **What this confirms, beyond the type.** The instance was RECREATED (new id,
> new subnet), the **AZ un-pin worked** — the box is in 1b, which is exactly
> what Quote 13 changed the shape for and what the 2026-08-07 capacity refusal
> could never have achieved on its own — and the 100 GB fresh-provision landed.
> The repo and the box now agree; the Quote 15 sweep was not aspirational
> bookkeeping.
>
> **The EIP was KEPT, not released.** It survived the recreate and is
> re-associated. Quote 10 approves release for the no-real-orders period via
> the bundled recreate, and the recreate has now HAPPENED with the address
> retained — so that lever is still un-taken and `docs/runbooks/eip-release.md`
> would now need its own fresh recreate. Stated because a reader could
> otherwise assume the bundle executed in full.
>
> **STILL UNRESOLVED (and now with a precise reason, not a shrug):** the
> 2026-07-31 flag that BOTH `STOP_EC2_INSTANCES` budget actions sit in
> `EXECUTION_FAILURE` **cannot be checked with this IAM identity** —
> `budgets:DescribeBudgetActionsForBudget` returns `AccessDeniedException` for
> `arn:aws:iam::208384284948:user/claude-code-agent`. That matters: if those
> actions are still failing, the budget kill-switch does not actually fire,
> and raising the ceiling to $100 never repaired it. Needs an identity with
> budgets read access.
(`aws ec2 describe-instances --filters Name=tag:Name,Values=tv-prod-app
--query 'Reservations[].Instances[].InstanceType'`) before any claim that the
box IS r8g.xlarge. What this change guarantees is narrower and worth stating
plainly: every surface now NAMES the same type, and nothing in the repo can
move the box away from it.

**Cost is unchanged by this quote** — the Quote 13 envelope stands
(~₹5,824–7,382/mo incl GST, ~6× the Quote 9 sub-₹1,000 target, knowingly

**Quote 17 (2026-08-19, disk THROUGHPUT/IOPS raise + budget ceiling raise — preserve EXACTLY, typos included):**
> "what the fuck as i already always told you nowheer the fuckign ticks lsos or not een a nano seocnd milliseocnd latency issues we hsodul afce dude alwyas veryhtign. needs to be fuckign preicse no ticks loss and no websocket discoencnt or no websocket reconenct issues dude okay? sow hatevr is eneded raise the ceiling dude okay? just raise whatevr yous aid and reocmemnded raise it accoridnly diue okay?"

Given in DIRECT response to a message that named the disk change, its price, and
its blocker: *"Raise disk throughput 3000/125 → 6000/500 · ~$20/mo · Removes the
~8-second writeback stall — the most likely single cause of falling out of the
fast lane · **Your kill-ceiling margin is $7.28. This needs the ceiling raised in
the same change.**"* The operator answered by ordering the ceiling raised to
whatever the recommendation requires. **This REVERSES his own answer given
minutes earlier** in the same session (he had selected "alarms + code fixes only,
disk waits for Saturday"); the later instruction governs, and the earlier choice
is recorded here so the reversal is auditable rather than silent.

**What this authorizes:** the root gp3 volume's **IOPS 3000 → 6000** and
**throughput 125 → 500 MiB/s**, and the budget kill-ceiling **$100 → $130**.
Nothing else — the instance stays r8g.xlarge (Quote 15, FINALISED), the AZ stays
un-pinned, the schedule stays 08:30–17:30 IST, and the 200 GB size from Quote 16
is unchanged.

**Why the operator is right about the cause.** gp3's baseline 125 MiB/s is not a
storage nicety here. `dirty_background_ratio = 3` on a 32 GiB host allows ~1 GiB
of dirty pages before writeback starts; at 125 MiB/s draining that is **~8
seconds of saturated device**. During it the ILP flush blocks, the frame drain
blocks behind it, the socket receive buffer fills, and Dhan — whose published
architecture skips a slow consumer forward to *"the latest available state"* —
drops the intermediate ticks at THEIR side, with no sequence number for us to
detect it. So this is not disk tuning; it is the most likely single mechanism by
which the operator's "not even a nanosecond of latency, no tick loss" mandate is
being violated today. 500 MiB/s takes the same drain to ~2 seconds. Evidence it
is already binding: the 2026-08-18 measurement recorded **74% NVMe utilisation at
3,121 writes/sec** — before the 25,000-instrument target and before depth
persistence.

**Honest cost.** gp3 charges $0.005/provisioned-IOPS above 3,000 and
$0.040/provisioned-MiB/s above 125: (6000−3000) × $0.005 = **$15.00** +
(500−125) × $0.040 = **$15.00** = **$30.00/mo**, ~₹3,043 incl GST — **higher than
the ~$20 the recommendation quoted**, because that figure was stated before the
per-unit split was derived. Recorded here rather than quietly absorbed: the
operator approved "whatever is needed", and this is what it actually costs. The
Quote 13/16 envelope moves ~₹6,739–8,297 → **~₹9,782–11,340/mo**. The Quote 9
sub-₹1,000 target was already breached ~7× and is now breached ~10×, knowingly.

**Ceiling $100 → $130 (the operative half of the quote).** The bill's high
estimate moves $82.72 → **$112.72**. The budget's AUTOMATIC action is
`STOP_EC2_INSTANCES` at 90% and 100% — so a $100 ceiling would put the 90% line
at $90, *below* the new bill, and the safety net would switch the trading box off
mid-session. $130 puts the 90% line at $117, above the $112.72 worst case with
$4.28 of room. That margin is THINNER than the $7.28 this change was called in to
fix, and it is stated plainly: the next cost increase of any size must raise the
ceiling in the same change, and there is no longer room to defer that.

**⚠ UNRESOLVED, carried forward from Quote 13 and NOT fixed by this raise:** the
2026-07-31 ruling recorded BOTH `STOP_EC2_INSTANCES` budget actions stuck in
`EXECUTION_FAILURE`, and the 2026-08-12 verification could not re-check them —
`budgets:DescribeBudgetActionsForBudget` returns `AccessDeniedException` for
`user/claude-code-agent`. **Raising a ceiling does not repair a broken action.**
If those actions are still failing then the kill-switch does not fire at all, in
which case this raise is protecting against a stop that cannot happen — which is
safer in the short term and worse in the long term. Needs an identity with
budgets read access.

**Quote 17b (2026-08-19, minutes later — ALL THREE, including the CPU isolation
I had recommended deferring; preserve EXACTLY, typos included):**
> "tehse issues are nwohere fuckigna cceptabel bro okay? so reosleva nd fix all
> of thes eude okay?"

Given in DIRECT response to the plain-English restatement of all three items,
including my own written caution on item 2: *"⚠️ The catch: it changes how the
program runs. Tomorrow is a trading day, and the box would wake up in an
arrangement it has never run in before. Saturday is the right day for this."*
The operator read that caution and ordered all three anyway. **That is his call
and it governs** — the deferral was my recommendation, not a constraint, and a
reaffirmed instruction ends the discussion.

**Additionally authorized by 17b: CPU isolation.** The host has 4 vCPU. Today
the tokio runtime defaults to `worker_threads = num_cpus = 4`, QuestDB holds a
`cpus: 3.0` quota, and NIC softirqs land wherever the kernel puts them — all on
the same 4 cores. This has already been measured biting: the compose file
records `nr_throttled 18,594 of nr_periods 85,149 = **21.8%**`. Authorized: an
explicit QuestDB `cpuset`, an explicit tokio worker-thread count, and NIC IRQ
steering away from the drain core. **Core 0 is NOT a valid pin target for the
drain** — it services network softirq, so pinning the decoder there puts it in
direct contention with the work it depends on (CLAUDE.md records this).

**The honest risk, which the operator accepted after reading it:** the box wakes
up tomorrow in a CPU arrangement it has never run in, on a trading day. If
tomorrow looks wrong, attribution between "the feed" and "the change" is harder.
Mitigation, since the risk cannot be removed: every part of this is
config-reversible without a code change, and the rollback is a single revert plus
a restart.

**What is NOT authorized by Quote 17/17b:** any instance-type change, any AZ
re-pin, any schedule change, any further EBS size change, live order fire, or any
edit to the §28 frozen indicator/strategy area. The operator's "no tick loss / no
disconnect" framing is the REASON for these changes, not a grant to make other
changes in its name.

**Quote 16 (2026-08-19, EBS grow 100 → 200 GB — preserve EXACTLY, typos included):**
> "Grow gp3 100 → 200 GB yes even icnrease this also dude okay?"

Given in DIRECT response to a three-row option table comparing r8gd.xlarge's local
NVMe against growing the gp3 volume, in which the gp3 row read verbatim **"Grow gp3
100 → 200 GB · survives daily stop ✅ · +$9.12/mo (~₹915) · **Yes** — online, one
command, `variables.tf` already permits up to 200"**. This is the fresh dated quote
§7 Mechanical Rule 3 requires before any EBS size change.

**What this authorizes:** the root gp3 volume grows **100 → 200 GB**. Nothing else —
the instance type stays r8g.xlarge (Quote 15, FINALISED), the AZ stays un-pinned, the
schedule stays 08:30–17:30 IST.

**Why the operator asked, and the correction that produced this quote.** The operator
proposed **r8gd.xlarge** instead, on the belief that it "has more memory". It does
not: r8gd.xlarge is **32 GiB, identical to r8g.xlarge** — the `d` denotes local NVMe
instance store (~237 GB), not additional RAM. And that NVMe is **wiped on every
instance stop**, while this box stops at 17:30 IST *every weekday*, so the current
day's data would be destroyed nightly — the precise outcome the operator's
same-session directive forbids (*"nothing shdou lbe missed or dleetd"*). r8gd was
already REJECTED on these grounds in Quote 13; the rejection stands and is
re-confirmed here rather than re-litigated.

**Honest cost.** gp3 storage is $0.0912/GB-month, so +100 GB = **+$9.12/mo →
~₹915/mo incl GST**. The Quote 13 envelope moves ~₹5,824–7,382 → **~₹6,739–8,297/mo**.
The Quote 9 sub-₹1,000 target was already breached ~6× and is now breached ~7×,
knowingly. The $100 kill-ceiling is unaffected: the bill's high estimate moves
$73.60 → $82.72, still under the 90% action line at $90 — but the margin narrows from
$16.40 to **$7.28**, which is worth stating plainly because the budget's AUTOMATIC
action is `STOP_EC2_INSTANCES` on the prod box. A further grow would need the ceiling
raised in the same change.

**Two things this quote does NOT do (Rule 11, no false-OK):**

1. **It does not grow the LIVE volume.** `root_block_device[0].volume_size` sits in
   the instance's `lifecycle.ignore_changes`, so `terraform apply` never touches the
   running root. The `variables.tf` default records FRESH-PROVISION intent; the live
   grow is the out-of-band online command
   (`aws ec2 modify-volume --volume-id <id> --size 200`, then grow the filesystem),
   or `scripts/aws-upgrade-instance.sh --ebs-size 200`. Until one of those runs, the
   live root stays at its current size.
2. **It is a ONE-WAY door.** gp3 grows online and can **never** shrink — a smaller
   `modify-volume` is refused and a 200 GB snapshot cannot restore into anything
   smaller. Reversing 200 → 100 would require a terminate-and-recreate. That is the
   reason the earlier 100 GB choice was deliberately taken over 250, and the same
   reason 200 should be the measured answer rather than a comfortable one: the first
   live session's real tick and depth volume is what should justify going further.

**What the extra 100 GB actually buys, arithmetically:** at the depth table's
measured 72 B/row the modelled load is ~21 GB/day at one snapshot/second and
~104 GB/day at five. 100 → 200 GB therefore buys roughly **+4.8 days at the low
estimate and +1 day at the high one**. It widens the runway; it does not remove the
need for the pressure-triggered archival landing alongside it, and neither does it
make a full disk impossible.
breached; kill-ceiling $100).

**Quote 18 (2026-08-22, HARD BILL CAP $125 — preserve EXACTLY, typos included):**
> "see the max poir aws bill cannot cost more than 125 dude okay? do you udnerstadn bro okay?"

Operator set a **hard maximum of $125/month on the AWS bill**. This is a
SPENDING constraint, and it is recorded here before any terraform change per
the rule-file-first law.

**Where the bill actually stands — MEASURED 2026-08-22, not estimated.** The
live account was read rather than the planning envelope trusted, because this
file's own history shows an unverified assumption becoming a recorded fact
(the 2026-08-12 "no AWS credentials" entry, which was false).

| Reading | Value | Source |
|---|---|---|
| July 2026, full month | **$28.96** | `ce get-cost-and-usage` — the OLD box; the r8g.xlarge recreate was 2026-08-12 |
| August 1–22 actual | **$39.80** | `budgets describe-budget` ActualSpend |
| AWS forecast, August | **$65.13** | same — LOW because August is mostly pre-upgrade |
| Aug 18 (weekday, pre-upgrade) | $2.50 | daily granularity |
| Aug 19 (weekday, pre-upgrade) | $2.62 | " |
| Aug 20 (IOPS raise applied mid-day) | $3.48 | " |
| **Aug 21 (Fri — first FULL weekday on the new disk)** | **$4.06** | " |
| Budget health | HEALTHY, limit $130 | `describe-budget` |

The $1.56/day step between 19 and 21 August matches the Quote 16/17 list-price
delta plus GST almost exactly ($1.26 × 1.18 = $1.49), which is what makes the
$4.06 trustworthy as the new steady-state weekday rate rather than a spike.

**Projected first FULL month on the current configuration:** 22 weekdays ×
$4.06 + ~8 weekend days × ~$2.76 (the weekday rate less EC2 compute and the
in-window Cost-Explorer polling, neither of which bills on a stopped box) =
**~$111–113/month, tax included**.

So the cap is **MET**, with roughly $12–14 of room. And the Quote 17 planning
figure of $112.72 turns out to have been **accurate**, not inflated — which
matters, because an intermediate draft of this section claimed the live data
showed it overstated the bill by ~$16 and used that to justify flipping the
ceiling to $125. That draft was wrong: it scaled the pre-upgrade month-to-date
forward instead of the post-upgrade daily rate, and AWS's own $65.13 forecast
looks reassuring for exactly the same reason. The terraform change was written,
caught by re-deriving from the daily series, and reverted before it was pushed.

**⚠ The trap, and why the ceiling was NOT flipped to 125 in the same breath.**
`limit_amount` is not a reporting threshold. The budget's AUTOMATIC actions are
`STOP_EC2_INSTANCES` on the prod box at **90% and 100% of it**. So:

| limit_amount | 90% action line | vs the $112.72 high side |
|---|---|---|
| $130 (live) | $117.00 | $4.28 of room |
| **$125** | **$112.50** | **AT or just under the projected $111–113 bill — the box gets stopped mid-month** |

Setting the ceiling to the operator's cap would arm an automatic shutdown of
the trading box inside a NORMAL high-side month. That is the exact reasoning
that rejected $30 on 2026-07-31 ("its 90% line already sits below the live
actual") and $80/$100 on 2026-08-08, and it applies here with a margin of
twenty-two cents.

**The real gap this quote exposes, which is worth more than the number.** The
live ceiling ($130) is now **above the operator's stated maximum ($125)**, so
the automatic guard would permit a month that breaches his limit before it
acted at all. The kill-switch is no longer aligned with the rule it is supposed
to enforce.

**Resolution path (not taken here — it needs an operator decision, not an
executor's).** Cutting **$0.22 or more** from the monthly high side makes $125 a
safe ceiling. The lever set, from the 2026-07-31 standing-waste record:

| Lever | Save/mo | Status |
|---|---|---|
| Release the Elastic IP | ~$3.60 | **Already approved** — Quote 10 (2026-07-19) authorizes release for the no-real-orders period. Execution is bundled with an instance recreate; the 2026-08-12 recreate happened and RETAINED the address, so it needs a fresh one. Takes the high side to ~$109.1 and gives $3.4 of margin under $112.50. |
| Cost Explorer polling | ~$2.38 | **Already minimal** — verified 2026-08-22: `hard_stop_guard` returns early when the box is stopped and only calls `mtd_usd` inside the up-window, so it bills ~198 requests/mo, not 720. Cutting it removes the in-session spend guard. |
| CloudWatch alarms/metrics | ~$3.27 | Cutting means losing pages; each has its own dated authorization. |
| Secrets Manager | ~$0.38 | Trivial, and on its own it clears the $0.22 with only $0.16 of margin — not margin. |

**What a PR that violates Quote 18 looks like (REJECT):** sets `limit_amount`
to 125 (or lower) while the recorded high-side estimate still exceeds 90% of
it; raises `limit_amount` above 125 without a fresh dated quote superseding
this one; or changes the ceiling in fewer than the three lockstep sites
(`budget.tf` + `budget-guards.tf` + `budget_digest.rs`).

**⚠ RE-TESTED LIVE 2026-08-22 and STILL BLOCKED — carried forward from Quotes 13
and 17:** the 2026-07-31 ruling found BOTH `STOP_EC2_INSTANCES` actions in
`EXECUTION_FAILURE`, and this session ATTEMPTED the read rather than
repeating the claim. The denial is verbatim: `User:
arn:aws:iam::208384284948:user/claude-code-agent is not authorized to perform:
budgets:DescribeBudgetActionsForBudget ... because no identity-based policy
allows the ... action`.

That wording matters. It is an IAM POLICY gap, not a missing credential — the
same identity successfully ran `sts get-caller-identity`, `budgets
describe-budget` and `ce get-cost-and-usage` minutes earlier. So the fix is one
action added to the agent's policy, not new keys. If those two actions are
still in `EXECUTION_FAILURE`, every ceiling number above is arithmetic about a
switch that does not throw, and this remains the single most important open
item on this whole surface.


**Quote 19 (2026-08-25, EBS grow 200 → 300 GB after a disk-full production halt — preserve EXACTLY, typos included):**
> "go ahead with your eocmmendation dude see clelary ntoe i never evr want ot face rpessure flushign espielclay entilrey rleated to db questdb evryhtign i shoduld alwyas achieve O(1) dude okay?"

Given in DIRECT response to a message recommending **200 → 300 GB, +$9.12/mo**,
presented alongside the measured evidence below and the explicit statement that
growing gp3 is a one-way door. This is the fresh dated quote §7 Mechanical
Rule 3 requires before any EBS size change.

**The incident this answers (MEASURED live, 2026-08-25, not estimated).** The
200 GB root volume filled during the session. QuestDB's O3 merge failed with
`CairoException: [28] No space left [size=70243632]` at **11:29 IST**, naming
`table=ticks~33` and `table=market_depth~34`, and **fourteen tables
WAL-SUSPENDED themselves**: `ticks`, `market_depth`, and every candle frame
from `candles_1s` to `candles_1d`.

| Table | sequencerTxn (accepted) | writerTxn (stored) | Behind |
|---|---:|---:|---:|
| `market_depth` | 244,651 | 214,743 | **29,908** |
| `candles_1s` | 272,941 | 267,868 | 5,073 |
| `candles_1m` | 272,415 | 267,260 | 5,155 |
| `ticks` | 142,410 | 137,899 | 4,511 |
| `order_audit` | 10 | 4 | 6 |
| `order_update_events` | 10 | 4 | 6 |
| `ws_event_audit` | 3,372 | 3,372 | 0 (healthy) |

**Why this was invisible, and it is the important half.** A WAL-suspended
QuestDB table **keeps accepting and ACKing ILP writes** while silently not
applying them. Every writer therefore reported success: the operator's manual
super order incremented `tv_order_update_events_rows_total{feed="dhan"}` and
`tv_order_audit_rows_total{event="placed"}` to 6 each, no persist-error counter
existed at all, and both tables were EMPTY. The operator found it by asking why
his order was not in any table — not by a page.

> **⚠ CORRECTED 2026-08-25 — the sentence that stood here was FALSE, and it is
> the exact class this file's own O(1) table warns about.** It read:
> *"`WAL-SUSPEND-01` exists for exactly this and did not fire; that gap is
> tracked separately from this quote."* Checked against the live log rather
> than repeated, the code **DID** fire, promptly, and it **DID** page:
>
> | evidence | value |
> |---|---|
> | coded `ERROR` events, 2026-08-25 | **70**, naming each suspended table (`ticks`, `candles_3m`, `candles_15s`, …) with `error_message: "bulk update failed and will be rolled back"` |
> | first fire | **11:23 IST** — the volume reached 100% at ~11:11, so ~12 minutes |
> | CloudWatch alarm | `tv-prod-errcode-wal-suspend-01`, `ActionsEnabled: True` |
> | alarm transitions that day | OK→ALARM **11:24 IST**, →OK 12:05, OK→ALARM **12:37**, →OK 13:02 |
>
> So detection worked and the operator was paged twice. What actually failed
> was the DISK, and separately the operator's own read of the situation — he
> found the empty tables by asking, which is true, but not because nothing had
> told him.
>
> **This row cost real work.** A later session read the false sentence, carried
> it into a live risk assessment as *"the alarm for it did not fire"*, and
> ranked "make WAL-suspension page you" as the next thing to build — work that
> was already done and shipped on 2026-07-10 (W2 PR#6). That is the same
> failure the `day_ohlc_tracker` row records on 2026-08-12: a stale row does
> not merely fail to warn, it **manufactures false findings**, and the cost is
> paid by whoever trusts it next.
>
> The durable lesson is narrow and worth stating: this file records what an
> incident FELT like from the operator's side, and that is legitimate — but a
> claim about whether a MECHANISM fired is checkable in one query
> (`aws logs filter-log-events --filter-pattern '"WAL-SUSPEND-01"'`) and must
> be checked before it is written, not inferred from the fact that nobody
> noticed the page.

**By 11:51 IST the box became UNMANAGEABLE.** `ssm send-command` began failing
in **0.001 s** with empty stdout and stderr — the agent cannot allocate the
scratch space needed to launch a shell. No remediation that requires the box
(deleting WAL segments, `RESUME WAL`) can run until the volume grows, which is
why the AWS-side `modify-volume` + reboot is the recovery path.

**Why more disk is the correct fix and not merely the easy one.** All three
provisioned dimensions were measured against CloudWatch peaks (22–24 Aug):

| Dimension | Provisioned | Measured peak | Used |
|---|---:|---:|---:|
| IOPS | 6,000 | 1,168 | **19%** |
| Throughput | 500 MiB/s | 107 MB/s | **21%** |
| **Size** | **200 GB** | **200 GB** | **100%** |

Only SIZE is exhausted. **The Quote 17 IOPS/throughput raise is therefore NOT
reverted to fund this**, despite the tempting 19%/21% headroom: `VolumeQueueLength`
peaked at **8.5** during the 09:24–09:34 IST open burst, which is the opposite
signal, and a 5-minute metric bucket cannot show a ten-second burst. Trading
provisioned I/O for size on an unexplained queue depth would be guessing.

**Why the retention design did not save it.** The archival chain is exactly the
shape the operator describes — current day hot, everything older verified into
S3 and dropped — and it RAN: `pressure_archive_enabled = true` starts an episode
at 75% used and shrinks the hot window to `pressure_hot_days = 2`. Two days is a
hard floor (today and yesterday are still being written, so the archiver's
verify cannot close the count→drop race on them). It archived everything it was
permitted to, the volume was still full, and it raised `STORAGE-GAP-05` and
STOPPED rather than dropping anything unverified. That is correct behaviour, and
it means **two days of data no longer fit in 200 GB** — a fact no retention
setting can change.

**What 300 GB buys, arithmetically.** Live usage is ~157 GB QuestDB + ~35 GB
frame WAL. At 300 GB the QuestDB working set sits at **~52%**, comfortably below
the 75% pressure trigger, which is what the operator's "never face pressure
flushing" requirement actually demands. (The 35 GB WAL figure is itself a defect
already fixed in PR #1804 — the ACTIVE segment set had neither an age nor a byte
bound — so the real post-deploy headroom is larger still.)

**⚠ Honest cost, including the part that does not fit.** gp3 storage is
$0.0912/GB-month, so +100 GB = **+$9.12/mo → ~₹920 incl GST**. Derived from
MEASURED daily cost rather than the planning envelope: the highest full weekday
on the current configuration is **$4.06** (Aug 21) and a weekend day is ~$2.48,
so a maximal month is 22 × 4.06 + 8 × 2.48 = **$109.16**, going to **$118.28**
with this grow. That is **under the Quote 18 hard cap of $125** — the constraint
the operator actually stated.

It is **NOT** under the budget's automatic action line. `limit_amount` is $130
and `STOP_EC2_INSTANCES` fires at 90% = **$117.00**, so a maximal month now
projects **$1.28 above the line that switches the trading box off**. This is the
precise trap Quote 18 documented and it is recorded here rather than absorbed:
the live budget today reads actual **$48.87** / forecast **$61.51**, so August
itself is nowhere near it, but a full month on this configuration is. Two levers
close it, both needing their own decision — releasing the Elastic IP (already
approved in principle by Quote 10, −$3.60/mo, execution bundled with an instance
recreate) or aligning `limit_amount` with the $125 cap, which cannot be done as
written because 90% of 125 is $112.50, BELOW the projected bill.

**⚠ RE-TESTED LIVE 2026-08-25 and STILL BLOCKED, for the fourth time:**
`budgets:DescribeBudgetActionsForBudget` returns `AccessDeniedException` for
`user/claude-code-agent`. Whether the two `STOP_EC2_INSTANCES` actions recorded
in `EXECUTION_FAILURE` on 2026-07-31 still fail is **Unknown**. That cuts both
ways and neither way is comfortable: if they are broken the $117 line above is
arithmetic about a switch that does not throw, and if they are fixed the box
gets stopped mid-month. A broken safety net is never a reason to cross a
threshold.

**One-way door.** gp3 grows online in one command and can **NEVER** shrink;
`modify-volume` refuses a smaller size and a 300 GB snapshot cannot restore into
anything smaller. Reversing this needs a terminate-and-recreate. `variables.tf`
validation permits 10–200 GB, so the ceiling moves to 300 in the same change.

**What Quote 19 does NOT authorize:** any instance-type change; any AZ re-pin;
any schedule change; any IOPS or throughput change in either direction; raising
`limit_amount`; live order fire; or any edit to the §28 frozen area. The
operator's "never face pressure flushing / always O(1)" framing is the REASON
for this grow, not a grant to make other changes in its name.
---

> **[ARCHIVED 2026-07-20]** §1 The rule (retired subscription contract) — moved verbatim to `docs/rules-archive/daily-universe-scope-expansion-2026-05-27-archive.md` (context-size incident; content unchanged).
## §2. The complete allowed set (POST-2026-05-27)

| WebSocket | Count | Endpoint | Allowed instruments | Mode |
|---|---|---|---|---|
| **Main feed** | **1** | `wss://api-feed.dhan.co?version=2&token=<JWT>&clientId=<ID>&authType=2` | Daily-fetched universe (~250 SIDs): all `IDX_I` rows where `EXCH_ID IN (NSE, BSE)` AND `INSTRUMENT == INDEX` (~30) + every unique `UNDERLYING_SECURITY_ID` referenced by `FUTIDX/OPTIDX/FUTSTK/OPTSTK` rows, resolved to its NSE_EQ row (~218) + ALL available monthly FUTIDX expiries of the 4 underlyings (NIFTY/BANKNIFTY/MIDCPNIFTY = NSE_FNO, SENSEX = BSE_FNO; typically ~12 contracts, envelope ≤24) per §36/§36.7 (2026-07-10) | **Quote (request code 17)** — 50-byte packets, gives day OHLC at fixed byte offsets |
| **Order update** | **1** | `wss://api-order-update.dhan.co` | Receives order events for orders WE place; filter `Source=P` | JSON, MsgCode 42 auth |

**Total live WebSocket connections to Dhan: 2** (UNCHANGED from prior lock).

**Universe size envelope (mechanical bound):** `MAX_DAILY_UNIVERSE_SIZE = 1200` (raised from 400 per §31, NTM expansion 2026-06-06). Boot HALTS if computed universe is outside `[100, 1200]`. Fits comfortably on 1 main-feed connection (Dhan cap = 5,000 SIDs/conn). The §36.7 FUTIDX all-months grant (2026-07-10) adds the vendor-listed monthly serials (~12 SIDs typical, ≤24 by envelope) to the subscription set — still trivially inside `[100, 1200]` (≈343 total).

> **⚠ CORRECTED + RAISED 2026-08-19 — the envelope is now `[100, 25,000]`, and the
> "Boot HALTS" sentence above has been FALSE since 2026-07-13.** Two separate things,
> recorded together because one hid the other:
>
> **(a) The halt does not exist.** The enforcing code was `build_daily_universe()`,
> deleted with the Dhan instrument-fetch chain on 2026-07-13. A workspace scan on
> 2026-08-19 found **zero production readers** of `MAX_DAILY_UNIVERSE_SIZE` or
> `MIN_DAILY_UNIVERSE_SIZE` — only comments and the ratchet that pins the value. The
> proof is live, not theoretical: the master-sourced flip on 2026-08-12 put **4,565
> SIDs** in the live set — nearly **4× over** the stated 1200 cap — and the lane booted
> normally, every trading day since, with nothing halting and nobody noticing. A
> documented boot-halt that cannot fire is the exact false-OK class this file's own
> O(1) table header warns about.
>
> **(b) The cap is raised 1200 → 25,000** per the 2026-08-15 full-universe
> authorization in `websocket-connection-scope-lock.md` ("2026-08-15 — FULL-MODE,
> FULL-UNIVERSE SUBSCRIPTION SCOPE"), whose REJECT list requires the constant, this
> rule file and the ratchet to move in lockstep — all three moved in the same change.
> 25,000 is not a round number: it is `5 connections × 5,000 instruments`, the
> main-feed subscription capacity.
>
> **What actually bounds the live set** is `DhanEndpointType::MainFeed
> .subscription_capacity()`, which `main.rs` passes to `resolve_live_universe` and
> which `plan_pool` enforces **fail-closed — refusing the WHOLE pool rather than
> truncating**. The constant is now numerically consistent with that real bound, so a
> reader who trusts the number is not misled about the SIZE even while it is not the
> thing doing the enforcing. The `[100, 1200]` text above is retained as the 2026-06-06
> audit record per house convention.

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

## §7. Instance lock — r8g.xlarge, MULTI-AZ (LOCKED 2026-08-08 per §0 Quote 13 — the 13-timeframe + tick-retention requirement, and the AZ un-pin that actually ends the capacity outage; supersedes the 2026-08-07 t4g.large + 2026-07-15 t4g.medium + 2026-06-30 r8g.large + 2026-05-29 m8g.large + 2026-05-27 t4g.large + 2026-05-18 t4g.medium locks)

> **2026-08-08 (Quote 13) — the AZ pin is REMOVED, and that is the load-bearing
> change.** Every prior instance lock in this section assumed the box lives in
> `${var.aws_region}a`. The 2026-08-07 t4g.large flip proved that assumption is what
> broke the box: AWS refused the NEW type in the SAME zone for the SAME reason. So the
> zone is now `var.availability_zone` over subnets in 1a/1b/1c, and a capacity refusal
> is a one-variable re-apply instead of days of downtime. **Any future instance-type
> change must keep the multi-AZ shape — re-pinning to a single zone is a REJECT.**

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
| Instance | **r8g.xlarge** — ARM Graviton4, **4 vCPU, 32 GiB RAM** (memory-optimised). *(Changed from t4g.large 2026-08-08 per §0 Quote 13 — to serve the 13-timeframe + current-day tick-retention requirement at ~25,000 instruments. `r` = 8 GiB/vCPU because the workload is memory-bound; `m8g` would force buying unused CPU to reach the same RAM, `r8gd`'s local NVMe is wiped on every stop (the box stops daily), and `r8i` would force an x86 rebuild of the whole ARM pipeline. Cost ~₹5,824–₹7,382/mo — the Quote 9 sub-₹1,000 target is BREACHED ~6× and knowingly accepted; see Quote 13 for the derivation.)* |
| **Availability zone** | **NOT PINNED** — subnets provisioned in `ap-south-1{a,b,c}`, instance zone selected by `var.availability_zone` (default `b`). *(2026-08-08, Quote 13. The old single-AZ pin at `main.tf:77` is what kept the box dark 2026-08-06→08: `describe-instance-type-offerings` confirms all 7 candidate types are offered in all 3 AZs, so the zone — not the type — was the blocker. Re-pinning to one zone is a REJECT.)* |
| Region | ap-south-1 (Mumbai) |
| Tenancy | Default (Shared) |
| Pricing | On-demand **$0.0224/hr** (ap-south-1, console-verified 2026-05-18 — re-verify at execution) — no Reserved / Savings Plan / Spot |
| Schedule | **Trading weekdays only (Mon–Fri), 08:30–16:30 IST auto** (start `cron(0 3 ? * MON-FRI *)`, stop `cron(0 11 ? * MON-FRI *)`) — narrowed back from 08:00–17:00 on 2026-06-05 per operator ("make the aws instance start and stop from 8.30 am till 4.30 pm"; supersedes the 2026-06-02 widening). Out-of-window runs = operator manual start. Weekends + holidays = OFF unless manually started. |
| EBS | gp3 **100 GB LIVE — VERIFIED 2026-08-12** via `describe-volumes` on the recreated instance `i-0c3fe906dad5492fc` (in-use, gp3). This row said **"30 GB LIVE"** until now, which was true of the OLD box: the Quote 13 fresh-provision of 100 GB landed with the instance recreate, and gp3's grow-only constraint stopped applying the moment the volume was provisioned fresh rather than modified. History: 10 GB (2026-05-29) → 30 GB → [50 GB approved 2026-07-13, never applied] → 30 GB ACCEPTED (Quote 9) → 20 GB pre-staged target (executor, 2026-07-15) → **100 GB actual (Quote 13, applied at the 2026-08-12 recreate)**. The 20 GB pre-stage is therefore SUPERSEDED and did not happen — recorded so nobody re-plans a shrink toward it. **AUTHORIZED 2026-08-19 (Quote 16): 100 → 200 GB.** The terraform default is raised in the same change; the LIVE volume is UNCHANGED until the out-of-band online grow runs (`root_block_device[0].volume_size` is in `lifecycle.ignore_changes`, so `terraform apply` never touches the running root). +$9.12/mo → ~₹915/mo incl GST, narrowing the $100 kill-ceiling margin from $16.40 to $7.28. One-way door: gp3 can never shrink. **APPLIED AND VERIFIED LIVE 2026-08-21** — `describe-volumes` on the running account (post-close, box stopped) returns `vol-0c6ab6e593e39d8c8`: **200 GiB gp3, 6000 IOPS, 500 MiB/s, in-use** on `i-0c3fe906dad5492fc` in ap-south-1b. So the Quote 16 size grow AND the Quote 17 IOPS/throughput raise are BOTH live; `describe-volumes-modifications` records the IOPS step 3000→6000 as `completed`, started 2026-08-20T12:15:58Z. The sentence above — "the LIVE volume is UNCHANGED until the out-of-band online grow runs" — was true when written and is now SUPERSEDED; it is kept per house convention so the sequence stays readable. **The filesystem followed the volume**, which is the half a `describe-volumes` cannot show: `tv_spill_dir_free_bytes` read **200.6 GB free at 09:30 IST on 2026-08-21**, a figure a 100 GB filesystem cannot report. **⚠ The volume id in this row's history is NOT the live one** — `vol-073ccaa417a0f344b` belonged to the retired box; re-describe before any volume operation rather than trusting a recorded id. |
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

### Bill 2026-08-08 (Quote 13) — r8g.xlarge · 100 GB · ~210 hrs · EIP kept

Weekday 08:00–17:00 = 22 × 9 = 198 hrs + manual/deploy starts ⇒ **~210 hrs**
(supersedes the 270-hr ceiling basis for THIS bill; 270 remains the ceiling if
weekends are run). EC2 rate DERIVED from the recorded r8g.large bill — see §0
Quote 13 for the arithmetic; AWS list may reach ~$0.24/hr, hence the range.

| Line | Calc | USD (low) | USD (high) |
|---|---|---|---|
| EC2 **r8g.xlarge** (app + Docker + QuestDB) | $0.166–$0.24/hr × 210 hrs | $34.86 | $50.40 |
| Elastic IP (24/7, KEPT — see Quote 13) | $0.005 × 720 | $3.60 | $3.60 |
| EBS gp3 **100 GB** (fresh provision) | $0.0912 × 100 | $9.12 | $9.12 |
| S3 cold (ticks + candles aged out) | grows with retention | $7.50 | $7.50 |
| CloudWatch alarms/metrics (the ~$2.7 of dated COST-NOTE alarms, itemised HERE unlike prior bills) | — | $2.70 | $2.70 |
| SNS → SMS (optional) | ~100 India msgs | $0.28 | $0.28 |
| **Subtotal (pre-GST)** | | **$58.06** | **$73.60** |
| **× ₹85/$** | | ₹4,935 | ₹6,256 |
| **+ 18% GST** | | **~₹5,824/mo** | **~₹7,382/mo** |

**Honest envelope for this bill:** ~6× the Quote 9 sub-₹1,000 target — **NOT met,
knowingly**. Releasing the EIP under Quote 10 saves ~₹360/mo. The S3 line is the
loosest estimate (it scales with the **Assumed** 25–80 M ticks/day, which swings
disk 3×) — measure on the first live day. The budget kill-ceiling moves $35 → $100
in lockstep (Quote 13); a ceiling below ~$82 would fire `STOP_EC2_INSTANCES`
mid-session at the 90% action threshold.

### Bill 2026-07-19 (SUPERSEDED by the 2026-08-08 bill above; retained as audit)

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
*(2026-07-31 ruling — Quote 11, §0 bullet: the kill ceiling is RAISED
$25 → **$35** after the live budget breached at 109.9% ($27.47 actual vs
$25) with BOTH AUTOMATIC `STOP_EC2_INSTANCES` actions stuck in
`EXECUTION_FAILURE` against the prod box. The < ₹1,000/mo TARGET is
UNCHANGED; the downward ladder is PAUSED, not cancelled, and resumes once
the ~$8.31/mo standing waste is cut. Full record: `aws-budget.md`
"OPERATOR RULING 2026-07-31".)*
*(2026-08-08 ruling — Quote 13: the kill ceiling is RAISED $35 → **$100**. The
plan's high estimate is $73.60 and the AUTOMATIC actions fire at 90%/100%, so any
ceiling below ~$82 would try to stop the box mid-session; $100 puts the 90% line at
$90. The < ₹1,000/mo TARGET is formally BREACHED by this ruling (~6×) and the
downward ladder stays PAUSED. **FLAGGED, UNRESOLVED:** the 2026-07-31 record has
BOTH `STOP_EC2_INSTANCES` actions in `EXECUTION_FAILURE`, and the 2026-08-08
executing session could NOT verify their current state —
`describe-budget-actions-for-budget` returned AccessDenied. Raising a ceiling does
not repair a broken action; the kill switch may still not fire. Needs a live check
with budget-action read access before the safety net can be claimed to work.)*

**Quote 14 (2026-08-08, the 9-hour window — preserve EXACTLY, typos included):**
> "Yes dude make it as 8.30 till 5.30 pm dude okay?  Go ahead ddue oaky"

Operator chose **08:30–17:30 IST** rather than the 08:00–17:00 I had proposed, and
that choice is what made the change safe. **Keeping the 08:30 START untouched
sidesteps the entire morning coupling** I had flagged as a blocker: the
start-watchdog arm + retry, market-open-readiness, the boot-heartbeat window open,
and the deploy-watchdog are all timed off 08:30 and every one stays correct. Only
the EVENING edge moves, and the boot-heartbeat false-page-every-morning risk
disappears entirely.

**APPLIED in the same PR** (four coupled sites, three of which would actively
FIGHT the schedule if left behind):

| Site | 16:30 → 17:30 | What a partial edit would have done |
|---|---|---|
| `main.tf` `daily_stop` cron | `cron(0 11)` → `cron(0 12)` | box stops an hour early |
| `hard_stop_guard::in_up_window` | `830..=1630` → `830..=1730` | runs HOURLY and **force-stops** any box outside its window ⇒ kills the box at 17:00, pages, silently cancels the paid hour |
| `start_watchdog::OPERATING_CLOSE_IST_MINUTES` | `17*60` → `17*60+30` | curfew guard **stops the box 30 min early** |
| `start_watchdog::STOP_TRIGGER_UTC_HOUR` + stop-verify cron | `11`→`12`, `cron(15 11)`→`cron(15 12)` (17:45 IST) | verify fires 45 min BEFORE the stop ⇒ false "auto-stop FAILED" page **every trading day** |

Plus the two operator-facing strings (console banner + offline message) and 5
existing tests whose boundaries encoded 16:30. Pinned by
`crates/aws-lambdas/tests/stop_window_lockstep_guard.rs` (6 tests, all three
force-stop sites bite-tested). Billing basis moves ~176 → **~198 hrs** (22 × 9),
inside the ~210-hr Quote 13 envelope — so the recorded bill is unchanged.

> **Note on instance schedule (2026-08-08, Quote 13) — SUPERSEDED SAME DAY by
> Quote 14 above, which APPLIED a 08:30–17:30 window. Retained as the record of
> why 08:00–17:00 was NOT taken.**
>
> The operator approved a weekday **08:00–17:00 IST** (9-hour) window, and the
> Quote 13 bill is costed at the resulting ~210-hr basis (22 × 9 = 198 hrs + manual
> starts). **The EventBridge crons are deliberately UNCHANGED in the PR that
> records this quote**, because the start time is not a standalone value — five
> Lambda/alarm schedules are timed as OFFSETS from the 08:30 start, and moving only
> `daily_start`/`daily_stop` would fire them against a box in the wrong state:
>
> | Coupled schedule | Now | Why it breaks on a naive shift |
> |---|---|---|
> | `start-watchdog` stop-verify (`cron(15 11)` = 16:45) | verifies the box stopped | would fire 15 min BEFORE a 17:00 stop and report a false failure |
> | `boot-heartbeat` window open (`cron(20 3)` = 08:50) | opens the boot alarm | an 08:00 start completes boot ~08:05; by 08:50 `tv_boot_completed` is >10 min stale ⇒ false page every morning |
> | `start-watchdog` arm + retry (`cron(0 3)`, `cron(15 3)`) | start + retry | fire after an 08:00 start — benign but semantically wrong |
> | `market-open-readiness` (`cron(15 3)` = 08:45) | pre-open readiness | still before 09:15 — benign |
> | `deploy-watchdog` (`cron(20 3)` = 08:50) | deploy check | benign |
>
> **Applying the 9-hour window therefore requires re-timing the whole chain in one
> change, with the boot-heartbeat window and the stop-verify as the two that
> genuinely misfire.** That is a scoped follow-up, not a line edit — and shipping
> the start/stop pair alone would trade ₹400–600/mo of savings for a false page
> every trading morning. The ~210-hr billing basis above is therefore the APPROVED
> TARGET; until the chain is re-timed the live basis stays ~176 hrs (08:30–16:30),
> which bills LESS, never more — the Quote 13 cost envelope is a ceiling, not an
> understatement.

> **Note on instance schedule (2026-05-29) — SUPERSEDED 2026-08-08 (Quote 14 → 08:30–17:30).**
> Trading WEEKDAYS only (Mon–Fri), **08:30–16:30 IST** auto start/stop.
>
> **CURRENT LIVE SCHEDULE: 08:30–17:30 IST Mon–Fri** (Quote 14).

> **Note on instance schedule (2026-05-29) — SUPERSEDED 2026-08-08:** trading WEEKDAYS only
> (Mon–Fri), **08:30–16:30 IST** auto start/stop. Weekends + NSE holidays
> = instance OFF unless the operator manually starts it. The 08:30 start
> gives the cold-boot + Step 1–6 auth + 08:45 CSV-fetch retry budget so
> the app is ready before 09:00 market open; 16:30 stop is ~1h after the
> 15:30 close (covers post-close digest + flush). Earlier plans
> (08:00–17:00 7-day, then 08:30–17:00 7-day) are superseded.

### Mechanical Rules (replaces aws-budget.md mechanical rules 1+6)

1. **Instance type is r8g.xlarge AND the AZ stays UN-PINNED. PERIOD.** *(2026-08-08,
   §0 Quote 13 — was t4g.large from 2026-08-07.)* Two things are locked by this rule,
   not one: the **type** (4 vCPU / 32 GiB Graviton4) and the **multi-AZ shape**
   (subnets in 1a/1b/1c, zone chosen by `var.availability_zone`). Re-pinning the
   subnet to a single AZ is a REJECT on its own, independent of the type — that pin
   is what caused the 2026-08-06→08 outage, and the 2026-08-07 type flip failed
   precisely because it changed the type while leaving the pin in place. Changing
   either requires:
   - Operator explicit approval with dated quote (see §0 Quote 13)
   - Update to this file
   - Update to `aws-indices-only-locked-architecture.md` §5
   - Update to `aws-budget.md`
   - Ratchet test `crates/storage/tests/instance_type_lock_guard.rs` updated to pin
     the new type
   - Update to `deploy/aws/terraform/variables.tf` `instance_type` default +
     validation, and `availability_zone` if the zone shape changes
   - Update to `scripts/aws-upgrade-instance.sh` `FROM_TYPE`/`TO_TYPE` defaults

   *(Superseded history below, retained as audit.)*

   ~~**Instance type is t4g.large. PERIOD.**~~ *(2026-08-07, §0 Quote 12 — was
   t4g.medium from 2026-07-15.)* Changing it (back to t4g.medium, to r8g.large,
   etc.) requires:
   - Operator explicit approval with dated quote (see §0 Quote 8)
   - Update to this file
   - Update to `aws-indices-only-locked-architecture.md` §5
   - Update to `aws-budget.md` (existing file marked SUPERSEDED)
   - Ratchet test `crates/storage/tests/instance_type_lock_guard.rs` updated to pin the new type
   - Update to `deploy/aws/terraform/variables.tf` `instance_type` default + validation
   - Update to `scripts/aws-upgrade-instance.sh` `FROM_TYPE` default

2. **Host memory budget for r8g.xlarge (32 GiB total) — the 13-TF + tick-retention
   target (~25,000 instruments)** *(2026-08-08, Quote 13 — supersedes the t4g.medium
   4 GiB budget retained below)*. Sizing Verified against source, not estimated:
   - Live candle slots: 13 TF × **128 B** (`LiveCandleState` — 11×f64 + 2×u64 + i64
     + 3×u32 = 124, padded) × 25,000 = **42 MB**. *(At the post-3.1 `TF_COUNT = 24`
     slot array it is 77 MB — still negligible.)*
   - Seal ring: 200,000 × ≤144 B (`seal_ring.rs:134` assertion) = **29 MB**
   - One day of ticks resident: **2.3–7.2 GB** (25–80 M × ~90 B — the tick count is
     **Assumed**, see Quote 13)
   - QuestDB: **8–16 GB** (`QDB_MEM_LIMIT` retuned at cutover — the 1g cap was sized
     for the 4-SID universe and is NOT valid here)
   - App remainder (indicator state, registry, ILP + audit buffers, tracing): 2–4 GB
   - OS + FS cache + kernel: 2–4 GB
   - **Total ~14–31 GB in 32 GiB.** Headroom is real but NOT generous at the top of
     the tick range.
   - **The t4g.medium Rule 2 FLAG is RETIRED by this change** — that flag recorded a
     ~2.5 GB predicted working set that did not fit 4 GiB. It fits 32 GiB.
   - **NEW FLAG (honest, unmeasured — Rule 11):** the 25–80 M ticks/day figure is
     **Assumed**; at the top of that range plus a QuestDB that grows past 16 GB, 32
     GiB gets tight. **The first live session at scale is the measured gate** — read
     `tv_process_rss_bytes` / RESOURCE-02 and `mem_used_percent`; r8g.2xlarge (64
     GiB) is the rip-cord. Graviton4 is NOT burstable, so `CPUCreditBalance` no
     longer applies (the t4g credit-starvation risk retires with the type).

   *(Superseded t4g.medium budget retained as audit.)*

   ~~**Host memory budget for t4g.medium (4 GiB total)**~~ — Groww-only runtime (~770-SID universe, 21 TFs):
   - QuestDB process: ~1.0 GB (`QDB_MEM_LIMIT=1g` — compose default + the on-box `deploy/docker/.env`, retuned by the downsize workflow's SSM step)
   - Tickvault app: ~700 MB actual **(the 2026-05-18 4-SID-universe measurement — NOT a ~770-SID measurement)** / 1.5 GB cap (see the FLAG below for what the retained sizing formula predicts at ~770 SIDs)
   - App: seal ring (200K seal cap, fixed): ~29 MB *(2026-07-18: replaces the deleted tick rescue-ring row — the 100K tick ring + its constant died with the dead tick writer, stage-2/4 sweeps; 200_000 seals × 144 B per `seal_ring.rs`)*
   - App: QuestDB ILP write buffer: 25 MB
   - App: 15+ audit-table buffers: 30 MB
   - Tracing / errors.jsonl rotation buffer: 100 MB
   - OS + FS cache + kernel TCP buffers: ~400 MB
   - **Total used: ~2.3 GB (app at the ~700 MB 4-SID actual) – ~3.1 GB (app at the 1.5 GB cap)** — the rows above sum to ~2.27 GB / ~3.07 GB (arithmetic corrected 2026-07-15; an earlier draft said "~2.6–3.1") *(2026-07-18: the seal-ring row replaced the 10 MB tick-ring row, +~19 MB — inside the ~ rounding, totals unchanged)*
   - **Headroom: ~0.9–1.7 GB** — above the 1 GB Linux kswapd floor only while the app stays at/under its cap. **FLAG (honest, unresolved — Assumed until measured; Rule 11, no false-OK):** the pre-downsize Rule 2 sizing formula this file has always carried (≈3.2 MB × SID for the 21-TF today+yesterday RAM-resident set) predicts an app working set of **~2.5 GB at ~770 SIDs** — with QuestDB at 1g that totals ~4.1 GB (2.5 app + 1.0 QDB + ~0.17 buffers + ~0.4 OS) and does **NOT fit in 4 GiB**. The ~700 MB "actual" and the formula cannot both hold at ~770 SIDs; **the first live session on t4g.medium is the measured gate** — read `tv_process_rss_bytes` / RESOURCE-02 and `mem_used_percent` before AND after cutover; if live RSS is materially above ~1.5 GB, 4 GiB does not fit and t4g.large (8 GiB) is the rip-cord. QuestDB at 1g serving today's ~770-SID Groww write load is likewise re-validated live (the old 1g-class budget served the 4-SID universe). BURSTABLE CPU: watch `CPUCreditBalance` after cutover.

3. **EBS = 200 GB gp3 (2026-08-19, Quote 16 — raised from the Quote 13 figure of 100;
   the paragraphs below are retained verbatim because every word of their reasoning
   still holds, only the number moved).** The grow was authorized after r8gd.xlarge was
   proposed and correctly rejected: its local NVMe is **wiped on every stop** and this
   box stops nightly, so it would delete each day's data — while EBS survives the stop.
   Cost +$9.12/mo (~₹915 incl GST); the $100 kill-ceiling margin narrows to $7.28, so a
   FURTHER grow must raise the ceiling in the same change. `variables.tf` validation
   already permits 10–200, so 200 sits exactly at the ceiling and going beyond it needs
   a validation edit plus its own dated quote. **The default documents fresh-provision
   intent only — the LIVE volume grows via the out-of-band online command**
   (`aws ec2 modify-volume --size 200` + filesystem grow, or
   `scripts/aws-upgrade-instance.sh --ebs-size 200`), never via `terraform apply`.

   *(Quote 13 text retained below — the sizing arithmetic, the shrink-impossibility and
   the archival doctrine are all unchanged by the raise.)*

   **EBS = 100 GB gp3 on the fresh volume (2026-08-08, Quote 13 — supersedes the 20 GB
   fresh-provision target; the LIVE root remains 30 GB until the recreate).** Sized for
   the 13-TF + tick-retention load: ticks ≈ 44–141 GB/mo (**Assumed** 25–80 M rows/day,
   swings 3×) + 13 TFs sparse ≈ 61 GB/mo, held ~30 days on disk with S3 archival beyond.
   Sparsity is Verified — `live_candle_state.rs:105` makes an unopened bucket a sentinel
   that emits nothing, which is the difference between ~46 M rows/day (2,050/sec, inside
   the ~5,000/sec envelope) and a dense 808 M rows/day (35,900/sec, 7× over).
   **100 and NOT 250 deliberately:** gp3 grows online in one command and can NEVER
   shrink, so the small side is the only reversible direction — if the real tick volume
   lands high, grow it live. `variables.tf` already permits 10–200 GB, so no validation
   change is needed. The AZ move forces a snapshot→restore (EBS is zone-locked), and the
   old volume is retained until explicit operator sign-off. Everything in the superseded
   text below about gp3's shrink impossibility, the `lifecycle.ignore_changes` on
   `root_block_device[0].volume_size`, the `EC2_INSTANCE_ID` secret rotation at recreate,
   and the >90d S3 archival **still applies verbatim**.

   *(Superseded 20 GB target retained as audit.)*

   ~~**EBS = 30 GB gp3 LIVE (verified 2026-07-19 — the 2026-07-13 approved 30→50 grow was recorded but never physically applied); 20 GB is the pre-staged fresh-volume TARGET**~~ (executor decision 2026-07-15, recorded in §0 under Quote 8 — NOT operator-quoted scope). gp3 grows online but can NEVER shrink: `modify-volume` refuses a smaller size and a larger snapshot cannot restore into a smaller volume (the 30 GB snapshot cannot restore into 20 GB), so 30 → 20 requires a volume/instance REPLACEMENT (terraform terminate-and-recreate in the operator's post-market erase window; the box is fully cattle-provisioned by `user-data.sh.tftpl`; the 2026-07-15 pre-downsize snapshot is the rollback, kept ~1 week; the GitHub secret `EC2_INSTANCE_ID` must be rotated to the new id at recreate time). Terraform `ebs_gp3_size_gb` default = 20 documents FRESH-PROVISION intent only — `root_block_device[0].volume_size` is in the instance `lifecycle.ignore_changes`, so `terraform apply` never touches the live volume. History: 10 GB → 30 GB (2026-05-29 Quote 6) → [50 GB approved 2026-07-13 (disk-pressure grow) — RECORDED but never physically applied; live verified 30 GB by `describe-volumes` 2026-07-19] → 20 GB target (2026-07-15) → **30 GB ACCEPTED (2026-07-19 Quote 9 — the 50 GB grow CANCELLED)**. **FLAGGED FOLLOW-UP (2026-07-19):** the unapplied grow means the 2026-07-13 82%-disk-pressure remediation never landed — applying it (or accepting 30 GB) is an operator/infra decision. *(RESOLVED same day by Quote 9: 30 GB is formally ACCEPTED, the grow is CANCELLED — the disk-pressure class is handled by code retention + S3 archival on the 30 GB root; any future grow needs a fresh dated quote. The 20 GB fresh-volume TARGET stays a separate un-quoted executor pre-stage — going below the accepted 30 needs its own operator go.)* The partition manager keeps auto-archiving partitions >90d to the S3 cold bucket (~4× cheaper per GB than EBS), so EBS holds only the hot window.

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

### §28.2 — NARROW LIFT for the u64 slot-mapping repair (operator-approved 2026-08-07)

**The verbatim operator authorization (2026-08-07, typed directly in-session):**

> "go ahead and fix and implement eveuthign dude okay>"

Given in DIRECT response to a message that named this lift as one of exactly two
explicit asks — quote: *"**'§28 approved'** → I fix #1, the single most
consequential defect in this entire session"* — alongside the summary of the
defect itself. That message is the scope this quote authorizes.

**The defect this lift repairs (Verified, red-team audit 2026-08-07).**
`IndicatorEngine::update` / `warmup_from_candles` / `warmup_count` index a flat
`states: Vec<IndicatorState>` of `MAX_INDICATOR_INSTRUMENTS` (25,000) with
`let sid = security_id as usize`, then bail on `sid >= self.states.len()`.
The §28.1 lift widened `security_id` to `u64` **and the id space subsequently
became NAMESPACE-BANDED** — Groww indices occupy `[2^62, 2^63)`
(`feed/groww/instruments.rs`), GDF `[2^60, 2^62)`, TrueData `[2^59, 2^60)`
(`truedata-feed-scope-2026-07-24.md` §9.5). Every banded id is therefore
astronomically `>= 25_000`, so **every** call returns `Default::default()`
(`is_warm = false`) and the strategy evaluator returns `Hold` — permanently,
for the entire live universe, **with no error, no counter, and no log line**.

§28.1 explicitly left the C2 `states` flat-Vec finding "TRACKED and
UN-remediated" as an I-P1-11 *collision* risk. After the banding it is no
longer a collision risk — it is a **guaranteed total silent no-op**, which is
strictly worse and was undocumented. Runtime-dead today only because the
trading pipeline spawns solely under `dhan_enabled || groww_enabled` and both
live feeds are retired; it would become a silent catastrophe the moment
strategies go live.

**Scope of THIS lift (narrow, mechanical):** ONLY the id→slot MAPPING may
change. An O(1)-average slot allocator (`HashMap<u64, u32>` + a dense
monotonic counter) translates a banded `security_id` into a dense index into
the SAME pre-allocated `states` Vec. Capacity exhaustion is FAIL-CLOSED and
LOUD (typed refusal + counter + coded error), never a silent miss.
**NO indicator math changes. NO strategy FSM logic changes. No
`IndicatorState` field changes.** The flat-Vec layout, its single startup
allocation, and every computation stay byte-for-byte identical; only the index
used to reach a slot is corrected.

**Still NOT remediated by this lift (recorded honestly, not silently carried):**
- **C1 warmup gate** — unchanged, still TRACKED.
- **I-P1-11 composite key** — the engine's public signature takes `security_id`
  alone, with no `ExchangeSegment`. Cross-FEED collision is now structurally
  impossible (disjoint namespace bands), but two instruments sharing a numeric
  id across SEGMENTS *within* one feed would still share a slot. Fixing that
  needs a signature cascade through the pipeline and is a SEPARATE lift needing
  its own dated quote.
- **NaN in `high`/`low`/`open` during REST warmup** poisons indicator state
  permanently (only `close` is guarded) — red-team finding #5, in the frozen
  area, deliberately NOT taken in this lift.

**Re-bless:** the `BOUNDARY_FILES` manifest in
`operator_boundary_indicator_strategy_guard.rs` is regenerated for the
post-repair tree. The guard stays ACTIVE — any further frozen-area edit beyond
this recorded lift fails the build again and needs its own fresh dated quote.

### §28.3 — NARROW LIFT for the STRATEGY-EVALUATOR slot repair (operator-approved 2026-08-07)

**The verbatim operator authorization (2026-08-07, typed directly in-session):**

> "fi everyhtugn dude oaky?"

Given in DIRECT response to a message that named exactly three fixes and asked the
operator to pick — quote: *"Say **\"fix the clock\"**, or **\"do 1 and 2\"**"* — with
the third row of that table reading **"fix the strategy" — brain offline**. The
operator answered "fix everything", which selects all three including this one. Same
authorization shape as the §28.2 quote ("go ahead and fix and implement eveuthign dude
okay>"), and recorded here BEFORE any code change, per this file's own protocol.

**The defect this lift repairs (Verified, 6-agent audit 2026-08-07, re-verified in
source by the executor before writing this).** §28.2 repaired `IndicatorEngine` — but
it explicitly scoped itself to the engine and stated "NO strategy FSM logic changes".
The strategy evaluator one stage downstream carries the **identical unrepaired cast**:

```rust
// crates/trading/src/strategy/evaluator.rs:52
let sid = snapshot.security_id as usize;
if sid >= self.states.len() || !snapshot.is_warm {   // states.len() == 25_000
    return Signal::Hold;
}
```

`engine.rs:406` populates the snapshot with `security_id: tick.security_id` — the RAW
banded id, NOT the dense slot the engine just resolved for its own use. Live ids are
namespace-banded (Groww `[2^62,2^63)`, GDF `[2^60,2^62)`, TrueData `[2^59,2^60)`), all
astronomically `>= 25_000`. So `evaluate()` returns `Signal::Hold` for **every live
instrument, permanently, with no error, no counter and no log line** — the exact silent
no-op signature §28.2 was written to eliminate, surviving one stage downstream.

Net effect after §28.2 alone: indicators compute **correctly** and the strategy
**discards every one of them**. Runtime-dead today (the trading pipeline spawns only
under `dhan_enabled || groww_enabled`, both retired), exactly as the engine defect was
— and, like it, a silent catastrophe the moment strategies go live. Unlike the engine
defect it is **not** covered by the §28.2 fail-closed counter
(`tv_indicator_slot_exhausted_total`), because the evaluator never reaches the engine's
allocator at all.

**Scope of THIS lift (narrow, mechanical):** ONLY the id→slot mapping used by the
evaluator may change. The engine's already-resolved dense slot is carried on
`IndicatorSnapshot` as a NEW field and the evaluator indexes by that. `security_id`
keeps carrying the real banded id for every downstream consumer that legitimately needs
it (audit rows, logs, persistence, cross-feed joins) — it is NOT repurposed.
**NO indicator math changes. NO strategy FSM transition logic changes. No
`IndicatorState` or FSM state-machine field changes.** Every condition evaluation and
every transition stays byte-for-byte identical; only the index used to reach a slot is
corrected.

**Still NOT remediated by this lift (recorded honestly, not silently carried):**
- **C1 warmup gate** — unchanged, still TRACKED.
- **I-P1-11 composite key** — the evaluator, like the engine, keys on `security_id`
  alone with no `ExchangeSegment`. Cross-FEED collision is structurally impossible
  (disjoint bands), but two instruments sharing a numeric id across SEGMENTS within one
  feed would still share a slot. Fixing that needs a signature cascade and its own
  dated quote.
- **NaN in `high`/`low`/`open` during REST warmup** poisons indicator state permanently
  (only `close` is guarded) — deliberately NOT taken in this lift.

**Re-bless:** the `BOUNDARY_FILES` manifest in
`operator_boundary_indicator_strategy_guard.rs` is regenerated for the post-repair
tree. The guard stays ACTIVE — any further frozen-area edit beyond this recorded lift
fails the build again and needs its own fresh dated quote.


### §28.4 — NARROW LIFT for the NaN/non-finite INGEST GATE (operator-approved 2026-08-09)

**The verbatim operator authorization (2026-08-09, typed directly in-session):**

> "yes take acre of eveyhtitgn enitlerey rbo okay?"

Given in DIRECT response to a ranked fix plan whose **P1** row read verbatim
*"NaN poisoning (#1) + clock-lock bugs (#2,#3) — Silent mistrades"*, presented
alongside the evidence below. That plan is the scope this quote authorizes.

**The defect this lift repairs (Verified, red-team audit 2026-08-09).**
`IndicatorEngine::update` widened `tick.day_high` / `tick.day_low` through
`f32_to_f64_clean` with **no finite check**. `f32_to_f64_clean`
(`crates/common/src/price_precision.rs:55`) deliberately passes NaN through, and
the Dhan quote parser is PROVEN to emit NaN — `crates/core/src/parser/quote.rs:382`
asserts `tick.day_high.is_nan()` in its own test.

Because Wilder smoothing is absorbing (`state.atr = state.atr*(1.0-wf) +
true_range*wf`), **one** NaN packet poisoned `atr`, `adx_tr_smooth`,
`vwap_cumulative_pv`, `prev_high`/`prev_low` **permanently for the process
lifetime**. `sanitize_nan_inf()` then clamped the SNAPSHOT to `0.0`, so the
strategy read `atr=0.0, adx=0.0, vwap=0.0` — plausible values, not an error — and
`reset_vwap_daily` / `reset_bollinger_daily` do **not** reset those fields, so the
daily reset never cleared it. A silent permanent mistrade with no error and no
counter: the exact false-OK class the charter forbids.

**This is the finding §28.2 explicitly deferred.** §28.2 recorded it verbatim as
*"NaN in `high`/`low`/`open` during REST warmup poisons indicator state permanently
(only `close` is guarded) — red-team finding #5, in the frozen area, deliberately
NOT taken in this lift."* This section is that deferred work, now authorized.

**Scope of THIS lift (narrow, mechanical):** ONLY an INGEST VALIDATION GATE may be
added. Non-finite (NaN/Inf) and non-positive LTP, and non-finite or negative
high/low, are refused BEFORE any state mutation — fail-closed, so the prior good
state survives intact and the instrument keeps computing on the next good tick.
The same gate is applied to `warmup_from_candles`, validating the CONVERTED f32
(which closes the NaN-high/low/open hole and the f64→f32 overflow hole together).
**NO indicator math changes. NO strategy FSM changes. No `IndicatorState` field
changes.** Every computation is byte-for-byte identical for inputs that were
already valid; only invalid inputs change behaviour, and only from
"silently poison forever" to "refuse, count, and log".

**Fail-closed and LOUD, never silent:** a refused tick increments
`tv_indicator_tick_rejected_total` and returns `is_warm: false` (which the
evaluator reads as unconditional `Hold`); the warmup path increments
`tv_indicator_warmup_candle_rejected_total`. Log lines are throttled to powers of
two so a poison storm cannot flood the sink. The gate runs BEFORE `slot_for`, so a
poison-only instrument cannot burn one of the 25,000 dense slots.

**One deliberate deviation from the literal brief, recorded rather than hidden:**
`high`/`low` of exactly `0.0` is ACCEPTED. `parse_ticker_packet`
(`crates/core/src/parser/ticker.rs:50`) builds its `ParsedTick` via
`..Default::default()` and the field docs state "0.0 for Ticker" — `0.0` is the
documented ABSENT sentinel, not a bad price. Rejecting it would have silently
dropped every Ticker-mode instrument, recreating the total-silent-no-op class
§28.2 exists to eliminate. Negative high/low IS rejected. Zero and negative LTP
ARE rejected. Pinned by `test_ticker_mode_zero_high_low_is_still_accepted`.

**Still NOT remediated by this lift (recorded honestly, not silently carried):**
- **C1 warmup gate** — unchanged, still TRACKED.
- **I-P1-11 composite key** — the engine still keys on `security_id` alone with no
  `ExchangeSegment`. Cross-FEED collision stays structurally impossible (disjoint
  namespace bands); two instruments sharing a numeric id across SEGMENTS within one
  feed would still share a slot. Unchanged by this lift; needs its own dated quote.
- **No `ErrorCode` variant** was created — the enum has no indicator-ingest
  variant, and adding one requires rule-file cross-reference edits outside this
  lift's scope. The refusal logs a bare `warn!` plus the counters above.
- **Neither new counter has a CloudWatch alarm or EMF allowlist entry**, matching
  the existing `tv_indicator_slot_exhausted_total`, which also has none. Flagged
  rather than assumed fine: today these are countable but not pageable.

**Re-bless:** the `BOUNDARY_FILES` manifest in
`operator_boundary_indicator_strategy_guard.rs` is regenerated for the post-repair
tree (`engine.rs` moves from 62108 bytes / 1662 lines to the post-gate size). The
guard stays ACTIVE — any further frozen-area edit beyond this recorded lift fails
the build again and needs its own fresh dated quote.
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

**Quote 19 (2026-08-25, budget ceiling raised to $150 to buy an O(1) guarantee — preserve EXACTLY, typos included):**
> "see evn if budget ened to be icnreased little bit extra also do it dude but i just need the O(1) workign solution always dude okay? yes fix evrythign. dude see i just need always O(1) dude i dotn need any slowness ddue irrespective of any situaitons dude okay?"

> "even if we need to reach the max 150 usd also let su gfo ahead dude but ensure to achieve alwyas O(1) dude okay?"

Given in DIRECT response to a report that the prod box's root volume was pinned at its
provisioned 500 MiB/s ceiling for an entire session (4,744 GB written for ~190 GB of
logical rows), that write latency degraded 0.86 -> 2.30 ms/op while the CPU idled at
13-27%, and that raising throughput 500 -> 1000 MiB/s would cost ~+$20/mo. This is the
fresh dated quote that Quote 18's own REJECT list requires before `limit_amount` moves
above 125, and it supersedes Quote 18's $125 hard cap.

**What this authorizes: a HARD MAXIMUM of $150/month.** Nothing else. The instance stays
r8g.xlarge (Quote 15, FINALISED), the AZ stays un-pinned, the schedule stays 08:30-17:30
IST, and no universe or scope change is authorized by it.

**Where the bill actually stands — MEASURED 2026-08-25, not projected.** Read live rather
than trusting the planning envelope, because this file's own history records an unverified
assumption becoming a recorded fact:

| Reading | Value | Source |
|---|---|---|
| August month-to-date actual | **$48.87** | `budgets describe-budget` ActualSpend |
| AWS forecast, August | **$61.51** | same |
| Live `limit_amount` | **$130** | same |

So the live bill is running well UNDER both the old $125 cap and the new $150 one, and the
$112.72 high-side figure Quote 17 planned against has not materialised. There is real room.

**The three lockstep sites** Quote 18 names (`budget.tf` + `budget-guards.tf` +
`budget_digest.rs`) must move together when `limit_amount` is changed. At $150 the 90%
`STOP_EC2_INSTANCES` action line sits at **$135**, which is $73 above the current forecast
— the widest margin this account has had since the ceiling ladder began.

**⚠ WHAT THIS QUOTE DOES NOT DO, and it is the important half.** The operator's stated
GOAL is O(1) with no slowness, and the budget is the means he offered, not the end. **Money
does not buy O(1) here.** Three findings from the same 2026-08-25 audit say so directly:

1. **The ILP flush runs ON the drain task** (`dhan_feed_stack.rs:2524`, `block_in_place`).
   A saturated disk therefore does not merely store data more slowly — it STALLS THE FOLD,
   which fills socket buffers, which is how a vendor that skips a slow consumer forward
   produces tick loss. Buying a bigger pipe shortens the stall; it does not remove the
   coupling. Decoupling the flush from the drain is the actual O(1) fix and it costs $0.
2. **The amplification is ~25x.** Raising throughput 2x against a 25x inefficiency buys one
   doubling of headroom and leaves the defect intact — and at the authorized 24,600-instrument
   target the same wall returns.
3. **A newly-identified suspect is a CONFIG value, not a capacity one:**
   `QDB_CAIRO_WRITER_DATA_APPEND_PAGE_SIZE = 16777216` (16 MiB) in
   `deploy/docker/docker-compose.yml`, multiplied across 24 candle tables x ~17 columns.
   If page-granular allocation is the amplifier, the fix is an env var and costs nothing.

**Therefore the EBS raise is AUTHORIZED but deliberately NOT TAKEN YET.** It will be taken
if and only if the measurement in plan Item 12 shows traffic still at the ceiling after the
amplification fix — at which point it lands with a measured number behind it rather than as
a guess. Spending first would hide the defect and the operator would still not have O(1).
Recorded this way so that a future session reads the authorization AND the reason it was
held, rather than treating an unspent authorization as an oversight.
