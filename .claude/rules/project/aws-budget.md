---
paths:
  - "deploy/docker/docker-compose.yml"
  - "deploy/aws/**"
  - "scripts/aws-*"
  - "crates/app/src/infra.rs"
  - "crates/trading/src/candles/seal_ring.rs"
  - "crates/trading/src/indicator/**"
  - "crates/trading/src/strategy/**"
---

# AWS Budget Enforcement — t4g.medium LOCKED ~₹1,022/mo

> **⚠ SUPERSEDED 2026-05-27 by [`daily-universe-scope-expansion-2026-05-27.md`](./daily-universe-scope-expansion-2026-05-27.md):** instance upgraded t4g.medium → t4g.large (8 GiB), bill ~₹1,022/mo → ~₹1,514/mo, cron 08:00 → 08:30 IST. Contents below retained as 2026-05-18 historical audit; current effective contract lives in the superseding file.
>
> **⚠ FURTHER SUPERSEDED → r8g.large 2026-06-30 (operator Quote 7 in [`daily-universe-scope-expansion-2026-05-27.md`](./daily-universe-scope-expansion-2026-05-27.md) §7):** instance upgraded m8g.large → **r8g.large** (Graviton4, 2 vCPU / 16 GiB), bill → ~₹2,919/mo incl GST (270 hrs, 30 GB EBS, +EIP kept). The current effective instance lock lives in that file's §7.
>
> **⚠ RE-SUPERSEDED → t4g.medium 2026-07-15 (operator Quote 8 in [`daily-universe-scope-expansion-2026-05-27.md`](./daily-universe-scope-expansion-2026-05-27.md) §7):** instance DOWNSIZED r8g.large → **t4g.medium** (Graviton2, 2 vCPU / 4 GiB), QuestDB QDB_MEM_LIMIT 4g → 1g. INTERIM bill → ~₹1,471/mo incl GST at 270 hrs with the live 50 GB root (gp3 cannot shrink; the 20 GB fresh-volume recreate — an executor pre-stage, NOT operator-quoted — drops it to ~₹1,197/mo; ~₹986/mo requires BOTH the ~176-hr auto-schedule basis AND the post-recreate 20 GB volume — on the live 50 GB root the ~176-hr figure is ~₹1,260, and ~₹986 is never the 270-hr one). EIP kept. The current effective instance lock lives in that file's §7. This file's original t4g.medium tables below remain 2026-05-18 historical audit (different universe/stack — do not reuse the ₹1,022 figure).
>
> **⚠ LIVE-VOLUME CORRECTION 2026-07-19:** the banner above's "live 50 GB root" premise was factually WRONG — `aws ec2 describe-volumes vol-073ccaa417a0f344b` (run live 2026-07-19 via the coordinator session) returned **30 GiB gp3 (3000 IOPS / 125 MiB/s), in-use**, attached to `i-0b956d0209231a48b` at `/dev/xvda` since 2026-05-24. The 2026-07-13 approved 30→50 GB grow (COST NOTE below) was RECORDED but **never physically applied**. Corrected interim bill: EBS $0.0912 × 30 = $2.74; subtotal $6.05 + $3.60 + $2.74 + $0.18 + $0.28 = $12.85 → ₹1,092 → ×1.18 GST = **~₹1,289/mo** at 270 hrs (was stated ~₹1,471/mo; the ~176-hr figure is ~₹1,077, was stated ~₹1,260). Post-recreate figures unchanged (~₹1,197 / ~₹986 — they assumed 20 GB). **FLAGGED FOLLOW-UP:** the disk-pressure remediation the grow was approved for is UNAPPLIED — the 82%-disk-pressure risk may recur; applying the grow (or formally accepting 30 GB) is an operator/infra decision, deliberately NOT taken in the docs-only PR carrying this note. *(RESOLVED same day by the 2026-07-19 OPERATOR RULING below: 30 GB is formally ACCEPTED and the 30→50 grow is CANCELLED.)* Full arithmetic + authority: `daily-universe-scope-expansion-2026-05-27.md` §7 (2026-07-19 correction note) + §0 (2026-07-19 approvals bullet).
>
> **⚠ OPERATOR RULING 2026-07-19 — 30 GB accepted, t4g.medium as-of-now, NEW HARD TARGET < ₹1,000/mo:** verbatim quote + the itemized sub-1K path live in the dedicated "OPERATOR RULING 2026-07-19" section below. The base bill alone (~₹1,077/mo at the ~176-hr auto-schedule basis) EXCEEDS the target — <₹1,000 is UNREACHABLE without at least one operator-gated lever; see the lever table.

> **UPGRADE 2026-07-21 (Quote 11): instance lock → m8g.large** (latest-gen Graviton4, 2 vCPU / 8 GiB fixed-performance, $0.06416/hr on-demand ap-south-1 — pricing-API-verified 2026-07-21, re-verify at execution). Honest all-in: **≈ ₹2,420/mo @ 270 hrs / ≈ ₹1,815/mo @ ~176 hrs** incl 18% GST (EIP + 30 GB EBS + S3 + SNS adders unchanged); r8g.large alternative (16 GiB, $0.08258/hr) ≈ ₹2,918 / ₹2,140; rejected 1-vCPU mediums m8g.medium ≈ ₹1,551/₹1,248 · r8g.medium ≈ ₹1,800/₹1,411. Quote 9's sub-₹1,000 hard target is SUPERSEDED for the instance line by Quote 11 (GDF RAM-first sizing + the 2-day t4g `InsufficientInstanceCapacity` drought outrank it; the 30 GB EBS acceptance stands). The $25 kill ceiling leaves ~$0.9/mo headroom at m8g.large 270 hrs (r8g.large exceeds it) — the lockstep triple raise (`budget.tf limit_amount` + `budget-guards.tf` BUDGET_KILL_USD + `budget_digest.rs` BUDGET_USD, e.g. → $30) is a FLAGGED operator follow-up, deliberately NOT flipped in the prep PR. Full record: `daily-universe-scope-expansion-2026-05-27.md` §0 Quote 11 + §7.

## OPERATOR RULING 2026-07-19 — sub-₹1,000/month hard budget target (30 GB accepted; grow CANCELLED)

**The verbatim operator demand (2026-07-19 — preserve EXACTLY, typos included):**

> "just 30 gn enough and onl yt4g medium as of now espeicall yentirkey it hsodul be kless than 1k per month dude oikay?"

**Meaning (recorded with the ruling):**
(a) the **30 GB root volume is formally ACCEPTED** — the 2026-07-13
approved-but-never-applied 30→50 GB grow is **CANCELLED** (the COST NOTE
2026-07-13 below and its 2026-07-19 correction are resolved by this ruling;
the 82%-disk-pressure class is now handled by code retention + S3 archival
on the accepted 30 GB, and any future grow needs a fresh dated quote);
(b) **t4g.medium stays locked as-of-now** (re-affirms the 2026-07-15
Quote 8 lock — no instance change);
(c) **NEW HARD BUDGET TARGET: total AWS bill < ₹1,000/month incl GST.**

### The itemized path to < ₹1,000 (evidence-backed arithmetic)

Recorded bases (daily-universe §7, 2026-07-19-corrected — Verified arithmetic):

- 270-hr ceiling basis: $12.85 → ₹1,092 → ×1.18 GST ≈ **₹1,289/mo**
- ~176-hr auto-schedule basis: EC2 $3.94 + EIP $3.60 + EBS-30GB $2.74 +
  S3 $0.18 + SMS $0.28 = **$10.74** → ₹913 → ×1.18 ≈ **₹1,077/mo**
- PLUS the un-itemised add-ons the §7 honest envelope already admits:
  the dated COST-NOTE alarm spend, recorded **~$2.7/mo ≈ ₹271/mo incl GST**
  (that recorded figure PREDATES the 2026-07-15→18 retirement notes below,
  ≈ −$2.5 in total — 0.50+0.10+0.70+0.10+0.40+0.70; with the 2026-07-14
  PR-C3 −$0.40 already netted inside the ~$2.7 record — so the LIVE number
  is likely materially lower — Unknown without Cost Explorer), and the
  2026-07-15 rollback snapshot
  `snap-090ed9c4f3df0ca61` at **~₹125/mo class** (Assumed magnitude —
  EBS snapshots bill on USED blocks at ~$0.05/GB-mo, so the 30 GB root
  bills its ~used-space ≈ $1.0–1.5 pre-GST ≈ ₹100–150 incl GST; a full
  30 GB would be $1.50 ≈ ₹150) until deleted.
- **Honest all-in today: ~₹1,473/mo at ~176 hrs (~₹1,685 at the 270-hr
  ceiling).** The base bill ALONE (₹1,077) already exceeds ₹1,000 — the
  target is NOT met by any amount of add-on trimming alone.

| # | Lever | Δ per month | Status |
|---|---|---|---|
| 1 | Delete rollback snapshot `snap-090ed9c4f3df0ca61` AFTER 2026-07-22 (its ~1-week rollback window ends Mon 2026-07-22) | −~₹125 (Assumed magnitude) | **SANCTIONED — schedule only, do NOT delete before 2026-07-22.** No auto-delete exists: the operator (or a dated follow-up session after Monday 2026-07-22) runs `aws ec2 delete-snapshot --snapshot-id snap-090ed9c4f3df0ca61` and records a dated note here. |
| 2 | Release the Elastic IP | −$3.60 → −₹306 pre-GST → **−~₹361 incl GST** | ~~**FLAGGED, OPERATOR-GATED — do NOT act.**~~ **2026-07-19 SECOND RULING (same day): release APPROVED for the no-real-orders period — but VERIFIED-UNSAFE-STANDALONE** (live describe evidence, coordinator session, 2026-07-19: the running instance's ENI eni-01fdeec2412f55587 has the ephemeral-public-IP attribute OFF — a launch-time ENI attribute AWS cannot enable post-launch — so releasing today = NO public IPv4 on the next stop/start = no SSM/feeds/deploys; the daily-universe §7 empirical claim CONFIRMED). Execution is therefore **BUNDLED with Lever 5** (the erase-window recreate — a FRESH launch in subnet subnet-00c8d06903d1482ea inherits `MapPublicIpOnLaunch=true` and gets an ephemeral public IP every start). Runbook: `docs/runbooks/eip-release.md` (verify-first order: recreate → prove ephemeral IP → release). Historical context of the old "do NOT act" status retained above via strikethrough — never deleted. |
| 3 | Trim the dated COST-NOTE alarm spend (menu below) | up to −~₹271 (recorded ceiling; live residual likely lower) | **FLAGGED, RULE-FILE-GOVERNED — change nothing here.** Each group landed under its own dated note; each trim needs its own dated retirement note. |
| 4a | SNS → SMS off (the bill table marks it optional) | −$0.28 → **−~₹28 incl GST** | Operator menu item — Telegram + Email fan-out remain (both free-tier). |
| 4b | S3 cold trim | −≤$0.18 → −≤₹18 | Effectively NOT a lever: SEBI 5y retention rules the archive; the line only grows. No real finding beyond the recorded $0.18. |
| 5 | 20 GB fresh-volume recreate (pre-staged 2026-07-15 executor decision; EBS $2.74 → $1.82) | −$0.92 → **−~₹92 incl GST** | **OPERATOR-GATED** (terminate-and-recreate in the erase window). NOTE: this ruling accepts **30 GB** as the live size; the 20 GB pre-stage remains a separate, un-quoted executor option — going below 30 needs its own operator go. **2026-07-19 SECOND RULING: the recreate is now the DELIVERY VEHICLE for Lever 2** — a fresh launch is the ONLY way this box gets ephemeral public IPs (the live ENI's launch-time attribute is off, verified 2026-07-19), so the EIP release and the 20 GB volume land together in ONE erase window (−₹361 − ₹92 in one action; runbook `docs/runbooks/eip-release.md`). |

**Lever-3 menu — the dated alarm groups (from the COST NOTES in this file):**

| Dated group | Recorded add | Trim caveat |
|---|---|---|
| Silent-feed hardening 2026-07-06 | +$1.50 (~₹150) | Several members ALREADY retired by the 2026-07-15/17 sweeps (dhan-lag alarm + 2 series, boundary-catchup-storm-dhan + 2 series) — the live residual of this group is far below $1.50. |
| Scoreboard PR-C 2026-07-11 | +$0.40 (~₹40) | Groww-lag alarm + series ALREADY retired (2026-07-15 Trap-A + 2026-07-17 dashboard tidy) — largely gone. |
| REST-audit gaps 2026-07-14 | +$0.60 (~₹60) | These page the ONLY remaining market-data pulls (spot-1m/chain legs + Telegram-drop) — trimming them blinds the REST legs. Not recommended. |
| Order-side cluster C 2026-07-14 | +$0.60 now (+$1.20 at Phase-1) (~₹60) | Order-path pagers (paper mode today). |
| Already-recorded retirements 2026-07-14→18 | −$0.40 −$0.50 −$0.10 −$0.70 −$0.10 −$0.40 −$0.70 ≈ **−$2.9** | Landed; they are why the live alarm spend is likely well under the recorded ~$2.7. |

**Which combinations reach < ₹1,000 (at the honest ~176-hr basis, all-in ~₹1,473):**

- Lever 1 alone: ~₹1,348 — **does NOT meet the target.**
- Lever 1 + full Lever 3 + 4a + 4b: 1,348 − 271 − 28 − 18 = **~₹1,031 — still misses** (and full Lever 3 is partly already spent by prior retirements).
- **Lever 1 + Lever 2 (EIP release): 1,348 − 361 = ~₹987 ✓** — meets the target while KEEPING all alarms + SMS.
- **Lever 1 + full Lever 3 + 4a + 4b + Lever 5 (20 GB recreate): 1,031 − 92 = ~₹939 ✓** — meets the target WITHOUT touching the EIP.
- At the 270-hr ceiling basis even Lever 1+2 misses (~₹1,199); every lever combined lands ~₹790 — but 270 hrs is the operator-set ceiling, not the auto-schedule actual; re-basing the recorded bill to ~176 hrs would itself need a §7 dated note.

**NO FALSE-OK — stated plainly:** the base bill (₹1,077 at ~176 hrs) exceeds
₹1,000 on its own, so **< ₹1,000/month is UNREACHABLE without at least one
operator-gated lever** (EIP release OR the 20-GB recreate combined with the
full alarm trim + SMS off). The sanctioned Lever 1 (snapshot deletion after
2026-07-22) is necessary in every combination but sufficient in none.
Operator decision list: (i) EIP release yes/no; (ii) which Lever-3 alarm
groups to retire; (iii) SMS off yes/no; (iv) 20 GB recreate go/no-go.

**Live Cost Explorer:** NOT consulted — the sandbox has no valid AWS
credentials (verified 2026-07-18: `UnrecognizedClientException`); the live
per-service split is the operator's daily budget digest / Cost Explorer
console. All ₹ figures above are the recorded list-rate arithmetic.

### OPERATOR RULING 2026-07-19 (SECOND, same day) — EIP release APPROVED for the no-real-orders period (verify-first, bundled with the recreate)

**The verbatim operator demand (2026-07-19 — preserve EXACTLY, typos included):**

> "until or unless we flip the real orders static ip is not needed due okay?"

**Meaning:** the Elastic IP is NOT needed until real orders flip on — its
release is APPROVED for the no-real-orders period, with the operator's
explicit safety order: **VERIFY outbound-without-EIP FIRST, release SECOND.**
This is the dated quote the Lever-2 row's old status ("requires its own dated
quote editing §7 first") demanded — daily-universe §7 carries the twin note
(Quote 10) in the same PR.

**Live verification verdict (live describe evidence, coordinator session,
2026-07-19 — supersedes tree-side prediction):**

| Fact | Evidence |
|---|---|
| Subnet IS auto-assign | `subnet-00c8d06903d1482ea` has `MapPublicIpOnLaunch=true` + explicit `0.0.0.0/0 → igw-00469f8a48d456a9c` route (genuinely public, no NAT) — matches `main.tf` lines 74–106 |
| BUT the LIVE ENI cannot use it | ephemeral-public-IP assignment is a LAUNCH-TIME ENI attribute; `eni-01fdeec2412f55587` (eth0 of `i-0b956d0209231a48b`, launched 2026-05-24 BEFORE the subnet flag landed ~2026-05-29) will NOT get a fresh ephemeral IP after EIP release, on stop/start or otherwise — AWS cannot enable the attribute post-launch |
| Current public IP | the EIP itself: `13.234.145.177` (`eipalloc-01d43d4debab9217b`, the account's ONLY EIP) |
| Verdict | **EIP release is NOT safe standalone** — releasing today bricks the box (no public IPv4 → no SSM, no REST pulls, no deploys). The 2026-05-31 `variables.tf` observation + daily-universe §7's "no public IP after stop/modify/start" claim are CONFIRMED live. |

**The sanctioned path (honors the verify-first order):** BUNDLE the release
with the erase-window instance RECREATE (Lever 5) — a fresh launch inherits
the subnet's auto-assign and gets an ephemeral public IP every start; only
the Dhan ORDER whitelist needs IP stability, and that is not needed until
live trading (the boot IP-verification code retired with the Dhan lane,
PR-C2 2026-07-13 — `ip_verifier` has zero production callers, Verified by
source scan 2026-07-19, so the app boots cleanly on an ephemeral IP with NO
code change). Full step-by-step: **`docs/runbooks/eip-release.md`**
(recreate → prove the fresh ENI mints an ephemeral IP → merge the
`enable_eip=false` terraform PR, whose path-filtered auto-apply lane
`.github/workflows/terraform-apply.yml` — plan on PR, apply on main push
outside market hours with 3 post-close cron retries — IS the release
mechanism; then `EC2_INSTANCE_ID` secret rotation + post-release checks +
the live-trading re-enable protocol: new EIP + Dhan setIP ≥7 days before
go-live).

**Bill recompute with the bundle sanctioned (all at the honest ~176-hr
all-in ~₹1,473 basis unless noted):**

- **Interim (now → the erase window; no EIP change yet):** Lever 1
  (−₹125) + Lever-3 trims (up to −₹271) + SMS off (−₹28) →
  1,473 − 125 − 271 − 28 = **~₹1,049** (full-trim floor; conservative
  trims that keep the REST-audit + order-side pagers land higher). Still
  over ₹1,000 — the interim does NOT meet the target; the window does.
- **Post-window (bundled L2 + L5 land):** 1,049 − 361 − 92 = **~₹596**
  (full trims) / 1,473 − 125 − 150 − 28 − 361 − 92 = **~₹717**
  (conservative trims, dead-group-only) — the **~₹600–720/mo class**,
  comfortably under the ₹1,000 target either way.
- **Even at the 270-hr ceiling basis (all-in ~₹1,685):** post-window
  1,685 − 125 − 271 − 28 − 361 − 92 = **~₹808** (full trims) /
  **~₹929** (conservative) — the target is met at BOTH bases once the
  bundle lands. The bundle therefore RESOLVES the "UNREACHABLE without at
  least one operator-gated lever" verdict above: the gated lever pair
  (L2+L5) is now approved and scheduled, pending only the erase-window
  execution + its in-window verification steps.
- The earlier "L1+L2 = ~₹987" line above assumed a standalone EIP release —
  superseded: L2 cannot land standalone (verified-unsafe); every
  target-meeting combination now routes through the bundle.

### Budget-alarm ceiling stepped down $55 → $25 (this PR) with a ratchet ladder to $10

The `budget.tf` `limit_amount` is a **KILL line**, not a mere alarm: 90%/100%
ACTUAL auto-STOP the box (native Budget Actions + the killswitch Lambda, which
also disables the morning start cron). Target arithmetic: ₹1,000 incl GST ÷
1.18 = ₹847 pre-GST ÷ ₹85/$ ≈ **$10/mo** — the eventual ceiling. But July 2026
is a MIXED month (r8g.large until the 2026-07-15 downsize): projected July
EOM ≈ $19–20 pre-GST (Assumed: ~88 r8g auto-hrs ≈ $7.3 + t4g remainder ≈ $1.5
+ EIP $3.60 + EBS $2.74 + alarms ≤$2.7 + S3/SMS $0.46 + snapshot ≈ $1.1), so
$10/$13/$18 NOW would cross the 90% kill line mid-July and stop the box.
Stepped value set in this PR: **$25** (90% line $22.5 stays above the ~$19–20
July projection; a 2.2× cut from $55). Dated ratchet ladder (each step = its
own PR editing `budget.tf limit_amount` + `budget-guards.tf BUDGET_KILL_USD`
+ `crates/aws-lambdas/src/budget_digest.rs BUDGET_USD` in lockstep, with a
dated cost note here):

- **$25 (2026-07-19, this PR)** — safe through the July mixed month.
- **→ $18** from the first full t4g.medium month (Aug 2026) — covers the
  270-hr all-in worst case (~$15.6 post-snapshot-deletion) with headroom.
- **→ $13** once Lever 1 + the chosen Lever-3 trims land (~₹1,300-class actual).
- **→ $10** once an operator-gated lever (EIP release, or recreate + trims)
  brings the actual bill under ₹1,000 (the ruling's line).

Residual (honest): `hard_stop_guard.rs` keeps `DEFAULT_BUDGET_KILL_USD = 55.0`
as its env-missing FALLBACK only — terraform always injects
`BUDGET_KILL_USD = "25"`, so the runtime kill line is $25; aligning the
fallback constant is a flagged follow-up (fail direction: a missing env var
kills later, never earlier).

## 2026-07-20 — capacity-incident emergency upsize + same-day revert (dated record)

On Mon 2026-07-20, 08:30–08:55 IST, the scheduled t4g.medium start of
`i-0b956d0209231a48b` failed with ap-south-1a `InsufficientInstanceCapacity`
across **9 attempts** (the 08:30 auto-start cron, the start-watchdog retries,
and manual starts). Emergency action: `ModifyInstanceAttribute` →
**t4g.large** at 03:26:43Z (08:56:43 IST) restored the start at
**08:59:27 IST** — ~15 minutes before market open. The session ran clean on
t4g.large; the box auto-stopped at 16:30:23 IST per the weekday schedule; the
instance was then **reverted to t4g.medium while stopped** (this record —
same evening, per the playbook below). EIP association
(`eipalloc-01d43d4debab9217b` → `13.234.145.177`) verified intact through
both type flips.

**Cost impact:** ≈ one session-day of the t4g.large-vs-t4g.medium delta
(~$0.20 pre-GST). The 2026-07-19 sub-₹1K posture above STANDS — this was a
bounded one-day emergency, not an instance-lock change; t4g.medium remains
the locked type (2026-07-15 Quote 8, re-affirmed by the 2026-07-19 ruling).

**Playbook (sanctioned rip-cord):** on an `InsufficientInstanceCapacity`
start failure of the locked type, a STOP-STATE type-flip to **t4g.large**
(same Graviton2 family, no AMI/volume/EIP change) is the sanctioned
emergency path to make the market open — then **revert to t4g.medium the
same evening** while the box is stopped, and land a dated record here. This
is a capacity-incident escape hatch only; any PERMANENT type change still
requires the full §7 dated-quote protocol in
`daily-universe-scope-expansion-2026-05-27.md`.

> **Authority:** Parthiban (architect). Non-negotiable.
> **Ground truth:** `docs/architecture/aws-indices-only-locked-architecture.md` §5 (instance lock 2026-05-18) and the 2026-05-20 CloudWatch-only decision below.
> **Scope:** Any file touching AWS deployment, infrastructure, Docker config, or cost-impacting changes.

## COST NOTE 2026-07-17 — dashboard tidy (−~$0.70/mo + 1 free-tier dashboard slot)

The dashboard-tidy PR (cleanup wave, Track B) retired the dead Dhan-lag
observability chain and the scoreboard dashboard (Verified against the
terraform diff in this PR; billing magnitudes Assumed at CloudWatch list
rates — active-series-hours were already $0-decaying since the producers
died with the live-WS retirements):

- **−1 alarm ≈ −$0.10/mo (Verified):** dhan-exchange-lag-p99-high
  (silent-feed-alarms.tf S3) — its only publisher
  (`run_dhan_lag_publisher`, feed_lag_monitor.rs) lost its spawn site +
  tick source with the Dhan live-WS lane deletion (PR-C2, 2026-07-13) and
  is deleted in this PR; a permanently-missing-data dead monitor (the
  groww-exchange-lag S4 precedent, 2026-07-15). Window-gate ALARM_NAMES
  trimmed 3 → 2 in lockstep (the same-day stage-3 sweep had already
  retired boundary-catchup-storm-dhan, 4 → 3).
- **−2 EMF allowlist series ≈ −$0.60/mo (names Verified; billing
  Assumed):** tv_dhan_exchange_lag_p99_seconds +
  tv_dhan_lag_samples_excluded_total (cloudwatch-agent.json +
  user-data.sh.tftpl, 17 → 15 names — the same-day stage-3 sweep had
  already removed the 2 dead aggregator names, 19 → 17).
- **−1 CloudWatch dashboard: ₹0 (Verified):** `tv-<env>-scoreboard`
  (dashboard.tf) — its Dhan-vs-Groww lag-trend widgets charted only the
  dead lag gauges; frees dashboard slot #2 of the 3-slot free tier.
- Dead-widget trim on the KEPT `tv-<env>-operator` dashboard (₹0):
  WebSocket-health / spill-dropped / DLQ-ticks widgets removed — their
  metrics have ZERO producers post live-WS retirements; their app-alarms.tf
  alarms are deliberately NOT touched here (flagged follow-up, dated notes
  in dashboard.tf).

Net ≈ **−$0.70/mo pre-GST (~−₹70/mo incl. 18% GST at ₹85/$)** — the real
gain is the freed dashboard slot + ~730 LoC of dead monitoring code.

## COST NOTE 2026-07-18 — tick-conservation retirement (−~$0.10/mo)

The tick-conservation retirement (dead-WS sweep follow-up, this PR) removed
the `tv-<env>-errcode-tick-conserve-01` log-filter alarm (−1 alarm ≈
−$0.10/mo, Verified against the terraform diff): its only emit site
(`crates/app/src/tick_conservation_boot.rs`, the 15:40 IST reconciler's
Leak arm) was deleted with the audit modules — every audit input died with
the dead tick chain in the stage-2 sweep (#1631), so the filter could never
match again (the ws-reinject-01 dead-filter precedent). No
`tv_tick_conservation_*` metric was ever in the EMF allowlist (grep-verified
— zero series delta). The `tick_conservation_audit` QuestDB TABLE is
retained (SEBI 5y). Dated notes in `error-code-alarms.tf` +
`observability-architecture.md`.

## COST NOTE 2026-07-18 — dead live-WS sweep stage 4 (−~$0.40/mo alarms; −4 EMF series)

The stage-4 dead-producer sweep (this PR) retired the 4 dead-tick alarm
chains whose emit sites died with the stage-2 tick-chain deletion
(2026-07-17 — `tick_persistence.rs` ring/spill/DLQ counters + the
`tick_processor.rs` post-close check); billing magnitudes Assumed at
CloudWatch list rates (active-series-hours decay to $0 once producers
stop publishing):

- **−4 alarms ≈ −$0.40/mo (Verified against the terraform diff):**
  `tv-<env>-spill-dropped`, `tv-<env>-dlq-ticks`, `tv-<env>-ticks-dropped`,
  `tv-<env>-late-tick-after-boundary` (app-alarms.tf; all ungated —
  no window-gate edit).
- **−4 EMF selector names ≈ −$1.20/mo at full density (Assumed; already
  $0 in practice — the producers stopped 2026-07-17):**
  `tv_spill_dropped_total`, `tv_dlq_ticks_total`, `tv_ticks_dropped_total`,
  `tv_late_tick_after_boundary_total` removed from both selector copies
  (cloudwatch-agent.json + user-data.sh.tftpl).

The seal-side loss pagers (seal-drop-alarm.tf + the AGGREGATOR-DROP-01
errcode alarm) are UNTOUCHED.

## COST NOTE 2026-07-17 — dead live-WS sweep stage 3 (−~$0.70/mo)

The stage-3 sweep (this PR) deleted the publisher-less 21-TF TICK aggregator
and its main.rs driver tasks — both live feeds are retired, so no tick
publisher exists and the aggregator's metrics lost their last possible
writers. Retired in lockstep (dated notes in `silent-feed-alarms.tf` S2 +
`app-alarms.tf` header + `market-hours-liveness-alarm.tf` +
`dashboard.tf`; billing magnitudes Assumed at CloudWatch list rates —
active-series-hours decay to $0 once producers stop publishing):

- **−1 alarm ≈ −$0.10/mo (Verified against the terraform diff):**
  `boundary_catchup_storm_dhan` (silent-feed-alarms.tf) — its metric
  `tv_boundary_catchup_total` was written only by the deleted aggregator's
  watermark catch-up sealer. Its window-gate ALARM_NAMES entry (gate now
  arms 3 alarms) and its dashboard widget + alarm-strip ARN left in the
  same PR.
- **−2 [host,feed] series ≈ −$0.60/mo (Assumed):** the second EMF
  `metric_declaration` (`^tv_boundary_catchup_total$` under [host,feed])
  deleted from `cloudwatch-agent.json` + `user-data.sh.tftpl`.
- **−2 main-list EMF names ≈ $0 marginal (dormant since PR-C2/stage-2):**
  `tv_aggregator_seals_emitted_total` + `tv_aggregator_close_pct_nonzero_total`
  removed from the host-only selector (17 names remain) — their emit sites
  (seal_routing.rs + the main.rs close-pct proof counter) died with the
  aggregator drivers.

Net ≈ **−$0.70/mo** — the seal-drop pagers (AGGREGATOR-DROP-01 errcode
alarm + `tv-<env>-seal-writer-dropped`, seal-drop-alarm.tf) are UNTOUCHED:
their subject, the storage seal-writer chain, survives with
`rest_candle_fold` as its sole producer.

## COST NOTE 2026-07-17 — dead live-WS sweep stage 1 (−~$0.10/mo)

The stage-1 zero-wiring dead-module sweep (operator directive 2026-07-17
via coordinator) removed the `ws-reinject-01` errcode log-filter alarm
(−1 alarm ≈ −$0.10/mo, Verified against the terraform diff in this PR):
its ONLY emit site (`crates/app/src/wal_reinject.rs`, retained un-consumed
since PR-C2 "pending the Phase C module cleanup") was deleted in that
cleanup, so the filter could never match again (the ws-gap-07 /
feed-stall-01 dead-filter precedent). Dated notes in
`error-code-alarms.tf` + `observability-architecture.md`. No other
alarm/metric/dashboard change in this sweep.

## COST NOTE 2026-07-15 — Groww live-feed retirement (Trap-A lockstep; net reduction)

The Groww live feed (sidecar + bridge + stall watchdog + lag publisher) is
retired (operator 2026-07-15: "remove the whole Groww live feed; keep only
spot 1m and option chain for both brokers; go"). Alarm/metric deltas
(Verified against the terraform diff in this PR; billing magnitudes Assumed
at CloudWatch list rates — active-series-hours decay to $0 once producers
stop publishing):

- **−3 alarms ≈ −$0.30/mo (Verified):** groww-ws-inactive +
  groww-stall-restart-storm (app-alarms.tf) + groww-exchange-lag-p99-high
  (silent-feed-alarms.tf S4).
- **−1 alarm ≈ −$0.10/mo (Verified, same-PR fix round):**
  aggregator-no-seals (app-alarms.tf section 9) — its metric lost its last
  live producer with the bridge deletion (Dhan broadcast publisher-less
  since PR-C2); a permanently-dead monitor the window gate kept arming
  (window-gate ALARM_NAMES trimmed 5 → 4 in the same edit).
- **−1 alarm + its fallback log metric filter ≈ −$0.10/mo + one sparse
  derived series (Verified):** the tv-<env>-feed-stall-restarts counter
  pager (feed-stall-restart-alarm.tf deleted whole).
- **−1 errcode alarm ≈ −$0.10/mo (Verified):** the "feed-stall-01"
  error_code_alerts entry (its ERROR-level emit site died with the stall
  watchdog; observability-architecture.md paging list updated in lockstep).
- **−4 EMF allowlist names ≈ −$1.20/mo at full in-session density (names
  Verified; billing Assumed):** tv_groww_ws_active,
  tv_feed_last_tick_age_seconds, tv_feed_sidecar_stall_restart_total,
  tv_groww_exchange_lag_p99_seconds.
- **+1 EMF name ≈ +$0.30/mo (Assumed):** tv_rest_1m_fire_heartbeat — the
  per-fire liveness gauge replacing the lag gauge **1:1 under the EXISTING
  tv-<env>-market-hours-liveness-missing alarm** (metric_name-only swap;
  0 new alarms; treat_missing_data="breaching" + the 09:20–15:35 IST window
  gate unchanged).

Net ≈ **−$0.50/mo alarms/filters + ≈ −$0.90/mo series (Assumed)** — inside
the $35/mo pre-GST budget alarm ceiling; the real saving is the ~30K-LoC
delete, not dollars. Honest residual (design Assumed sound, not
live-simulated): the heartbeat is deliberately NOT pre-registered at boot —
the first set at the 09:16:01 IST fire is the session-start signal; a day
where BOTH per-minute REST spot legs are disabled/dead pages the liveness
alarm ~09:25 IST — the designed loud outcome (zero in-session capture), not
a false page.

## COST NOTE 2026-07-14 — REST-audit alarm gaps (GAP-01/03/05, +~$0.60/mo)

The 2026-07-14 REST-pipeline adversarial audit
(`docs/audits/2026-07-14-rest-pipeline-adversarial-audit.md`) found the
REST-leg paging chain was app-emitted Telegram ONLY (GAP-01/GAP-03) with no
alarm on Telegram drops themselves (GAP-05). Added:

- **+5 errcode log-filter alarms ≈ $0.50/mo** (`error-code-alarms.tf`):
  `auth-gap-05-remint-failed` (mint-FAILURE arm only — `$.cooldown_skip
  IS FALSE` scoped; excludes the noise-lock H3 non-terminal
  cooldown-skip lines), `spot1m-01-escalation` + `chain-02-escalation`
  (`stage="escalation"` once-per-episode edges only), `chain-01`,
  `chain-04-warmup`. Their log-derived metrics are sparse/dimensionless
  (billed only in hours a code fires — near-free).
- **+1 counter-delta alarm ≈ $0.10/mo** (`telegram-drop-alarm.tf`):
  `tv-<env>-telegram-drops` on `tv_telegram_dropped_total` (Sum ≥ 3 per
  900s, metrics-log delta-extraction house pattern). The derived metric is
  sparse until the flagged crates-side pre-registration lands (near-free).

Total **≈ $0.60/mo pre-GST (~₹60/mo incl. 18% GST at ₹85/$)** — inside the
$35/mo pre-GST budget alarm ceiling and the ~₹3,101/mo envelope.

## COST NOTE 2026-07-06 — Silent-feed alerting hardening (+~$1.50/mo)

The 2026-07-06 incident (Dhan feed degraded ALL day — lag p99 46s/max 199s,
29-67 of 776 instruments silent every minute, 125 SLO crossings in the
0.94-0.95 band, 9k-11.5k BOUNDARY-01 catch-up seals/10min — with ZERO pages)
added, per `deploy/aws/terraform/silent-feed-alarms.tf` +
`deploy/aws/terraform/app-alarms.tf` (tick-gap retune 100 → 40 PROVISIONAL —
round-3 correction 2026-07-08: 25 sat below the documented ~33 always-silent
healthy floor and would have paged every healthy day):

- **+4 custom-metric series ≈ $1.20/mo:** 2× `tv_boundary_catchup_total`
  under `[host, feed]` (dhan + groww, second EMF declaration), 1×
  `tv_dhan_exchange_lag_p99_seconds`, 1× `tv_dhan_lag_samples_excluded_total`.
- **+3 alarms ≈ $0.30/mo:** realtime-guarantee-degraded (0.80-0.95 dead-band),
  boundary-catchup-storm-dhan (PROVISIONAL 2000/5m ×2 — re-ratchet after one
  observed trading week), dhan-exchange-lag-p99-high (>10s ×10min). All
  market-hours-gated (09:20-15:35 IST window Lambda), all
  `treat_missing_data = notBreaching`.

Total **≈ $1.50/mo pre-GST (~₹150/mo incl. 18% GST at ₹85/$)** — inside the
$35/mo pre-GST budget alarm ceiling and the ~₹2,919/mo envelope.

## COST NOTE 2026-07-11 — Groww exchange-lag visibility (scoreboard PR-C, +~$0.40/mo)

The dual-feed scoreboard PR-C added, per `deploy/aws/terraform/silent-feed-alarms.tf` S4:

- **+1 custom-metric series ≈ $0.30/mo:** `tv_groww_exchange_lag_p99_seconds`
  (the Groww mirror of the Dhan lag gauge — its OWN EMF name, the 27th
  allowlist entry; the Groww exclusion/clamp counters stay /metrics-only, ₹0).
- **+1 alarm ≈ $0.10/mo:** groww-exchange-lag-p99-high (>5s ×10min,
  window-gated 09:20-15:35 IST like the Dhan one; the window-gate Lambda now
  arms 12 alarms).
- **+1 CloudWatch dashboard: ₹0** (slot 2 of the 3 free dashboards —
  `tv-<env>-scoreboard`, Dhan-vs-Groww lag trends).

Total **≈ $0.40/mo pre-GST (~₹40/mo incl. 18% GST at ₹85/$)** — inside the
$35/mo pre-GST budget alarm ceiling and the ~₹2,919/mo envelope.

## COST NOTE 2026-07-14 — PR-C3 tick-gap retirement (−~$0.40/mo)

PR-C3 (tick-gap detector deletion, operator Q4-ii 2026-07-13) removed the
`tv-<env>-tick-gap-instruments-silent` alarm (−1 alarm ≈ −$0.10/mo) and the
`tv_tick_gap_instruments_silent` custom-metric series from the EMF allowlist
(−1 series ≈ −$0.30/mo) — the gauge producer was deleted with the Dhan WS
lane, so both would have been dead monitors. Dated notes in
`deploy/aws/terraform/app-alarms.tf` + `market-hours-liveness-alarm.tf`.

## COST NOTE 2026-07-13 — EBS 30→50 GB (+~₹170/mo incl GST)

Prod disk-pressure remediation (operator pre-approved 2026-07-13): the root fs
hit **82%** on 2026-07-13, growing ~2.5–3.6 GB/trading-day with ZERO
reclamation (partition manager `detached=0` every run; S3 archive leg never
fired) — the 50 GB gp3 grow is the pressure-relief backstop alongside the
code retention fixes. EBS line $2.74 → $4.56 ($0.0912 × 50), bill ~₹2,919 →
**~₹3,101/mo incl GST** — still under the $35/mo pre-GST budget alarm. The
box's S3 write for Groww-capture archival needed NO IAM change (the instance
role already has Put/Get/List on the whole `tv-<env>-cold` bucket). The
effective contract lives in `daily-universe-scope-expansion-2026-05-27.md` §7
(Mechanical Rule 3); the live grow is `scripts/aws-upgrade-instance.sh
--ebs-size 50` (online) — terraform's `ebs_gp3_size_gb=50` documents
fresh-provision intent only (`volume_size` is in `lifecycle.ignore_changes`).

**2026-07-19 correction — THIS GROW NEVER PHYSICALLY APPLIED:** live
`describe-volumes` (2026-07-19, coordinator session) shows the root volume
still **30 GiB gp3** — the approved `--ebs-size 50` command above was never
actually run against the box, so the +~₹170/mo EBS delta never materialized
and the pressure-relief backstop this note approved is **NOT in place**.
LOUD FLAGGED FOLLOW-UP: the 2026-07-13 disk-pressure class (root fs 82%,
~2.5–3.6 GB/trading-day growth) may recur on the 30 GB root — executing the
approved grow (or formally accepting 30 GB now that the Dhan WS lane +
Groww live feed retired and reduced the write load) is an operator/infra
decision, deliberately NOT taken in the 2026-07-19 docs-only correction PR.
**RESOLVED 2026-07-19 (same day, OPERATOR RULING above): 30 GB formally
ACCEPTED — this grow is CANCELLED** ("just 30 gn enough…"); any future grow
needs a fresh dated quote.
See the header banner correction + `daily-universe-scope-expansion-2026-05-27.md`
§7 for the corrected bill (~₹1,289/mo interim).

## COST NOTE 2026-07-14 — Order-side observability, cluster C (+~$0.60/mo now, ~$1.20/mo ceiling at Phase-1)

Order-side audit tables + alert-sink wiring + alarms (order_audit/pnl_audit rebuild, OMS→Telegram
bridge, orders-placed storm pager, arm-on-arrival fill-lag/daily-loss alarms), per
`deploy/aws/terraform/order-side-alarms.tf`:

- **+1 custom-metric series ≈ $0.30/mo:** `tv_orders_placed_delta_total` (derived, metrics-log
  filter on `/tickvault/<env>/metrics` — dense from the main.rs pre-registrations; the log filter
  itself is free). NO new EMF-published series bill today: the 2 new allowlist names
  (`tv_daily_pnl`, `tv_order_fill_lag_seconds`) are DORMANT — their emit sites ship with
  cluster A / Phase-1, so zero datapoints = $0.00 until then, then ≈ +$0.60/mo (noted here in
  advance so that PR needs no new cost note for them).
- **+3 alarms ≈ $0.30/mo:** orders-placed-storm (armed), daily-loss-breach (armed, structurally
  silent in dry-run — missing gauge + notBreaching), order-fill-lag-high (actions_enabled = false
  until Phase-1 arming). The pre-existing orders-rejected alarm is fixed at $0 (ok_actions
  removed + counter pre-registered — it was dead for single-rejection sessions).
- **Dashboard: ₹0** — one widget row appended to the EXISTING `tv-<env>-operator` dashboard;
  free-tier dashboard slot 3 deliberately NOT consumed.
- **Log-ingestion delta:** 3 newly-dense counter series ≈ a few hundred bytes/min into the
  metrics log group — noise inside the 5 GB free tier.

Total **≈ $0.60/mo pre-GST now (~₹51/mo incl. 18% GST at ₹85/$), ≈ $1.20/mo at Phase-1** —
inside the $35/mo pre-GST budget alarm ceiling and the ~₹3,101/mo envelope.

> **[ARCHIVED 2026-07-20]** 2026-05-18/2026-05-20 historical body (CloudWatch-only decision, t4g.medium lock narrative, ₹1,022 bill, schedule, mechanical rules, risks, RAM-first architecture, coverage table, automation charter — retained as 2026-05-18 historical audit per the top banner; current contract = daily-universe-scope-expansion §7 + the 2026-07-19 rulings above) — moved verbatim to `docs/rules-archive/aws-budget-archive.md` (context-size incident; content unchanged).
