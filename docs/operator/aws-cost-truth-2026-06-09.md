# AWS Cost — The Real Truth (2026-06-09)

> Operator asked: *"is this AWS budget cost correct or wrong? Who is tracking
> it, how is it calculated? Every day should give the precise AWS cost. I need
> guarantee + assurance, as an easy comparison table."*
>
> This document answers all four — every number is anchored to a `file:line` in
> the repo or a cited AWS price. Where I cannot confirm a number from this
> sandbox (no `aws` CLI), I say so plainly. **The ONLY ground-truth for actual
> spend is your AWS Cost Explorer — which a Lambda already reads daily.**

---

## 1. Who is tracking the cost, and how? (you DO have a tracker — two, in fact)

| # | Tracker | What it does | Source of the number | Where you see it | file:line |
|---|---|---|---|---|---|
| A | **AWS Budgets** `tv-prod-monthly-budget` | A hard **$25/month** budget on everything tagged `Project=tickvault`. At **80% forecast** → emails you. At **100% actual** → SNS → **kill-switch Lambda STOPS the EC2 box** + Telegram page. | AWS's own billing engine (authoritative) | AWS console → Billing → Budgets; Telegram on breach | `budget.tf:20,43-49` |
| B | **Daily Budget Digest** Lambda `tv-prod-daily-budget-digest` | Every **weekday 17:30 IST** reads **real Cost Explorer** spend, converts to ₹ (×85 ×1.18 GST), posts to Telegram: *yesterday ₹X / MTD ₹Y / forecast / days-left*. | **Cost Explorer `get_cost_and_usage`** (real, not an estimate) | Telegram, weekday evenings | `budget-guards.tf:40-78,146` |

**So the cost IS tracked from real AWS billing data — not a guess.** If you are
not seeing the daily Telegram digest, the likely reason is that **Terraform/deploy
has been failing** (the same syntax bug I just fixed in PR #1070), so this Lambda
may not be deployed yet — OR the SNS→Telegram route isn't wired to your chat.
That is the thing to verify, not the math.

---

## 2. Is the cost "correct or wrong"? — The honest verdict

**The digest's method is correct (real Cost Explorer data). After verifying the
Terraform, the bill is ROUGHLY RIGHT — my first pass wrongly flagged the Elastic
IP as a leak; it is actually mandatory + operator-approved. The two remaining
items are small:**

| # | Item | Finding after verification | Cost impact | Action |
|---|---|---|---|---|
| ✅ 1 | Elastic IP | **NOT a leak.** You enabled it on 2026-05-31 (`variables.tf:60-63`) because after the t4g→m8g manual upgrade the instance ENI has auto-assign-public-IP **OFF** — without the EIP the box has **no internet at all** (can't reach Dhan or SSM). Mandatory. | ~₹300/mo (necessary) | **keep** — it's load-bearing. (Stale subnet comment `main.tf:62-66` should be updated to match.) |
| 🟠 2 | CloudWatch alarms | **25 alarms deployed** (10 free) — real, but removing monitoring on a live trading box to save ₹150 is a bad trade. | +~₹150/mo | keep, or prune only clearly-redundant ones |
| 🟡 3 | Digest vs budget mismatch | Digest target = **₹2,000** (`budget-guards.tf` `BUDGET_INR`) but the AWS Budget that actually stops the box = **$25** (≈ ₹2,508 w/GST). The "% of target" you see ≠ the kill-switch trigger. | confusing, not costly | align the digest to $25 |

**The one number I still cannot confirm from here:** the m8g.large Mumbai hourly
rate. The rule file says **$0.06416/hr** (you console-confirmed); public
aggregators show m8g.large ≈ **$0.0898/hr** (us-east-1; Mumbai usually equal or
higher). **The daily digest shows the true number — trust that over any estimate.**

---

## 3. The precise per-day cost (verified prices, m8g.large + 30GB + EIP, weekday 08:30–16:30 IST)

Two columns because the instance hourly rate is unconfirmed (see §2). Pick the
column your digest matches.

| Cost item | Rate (cited) | Per **trading day** (box ON 8h) | Per **off day** (box OFF) |
|---|---|---|---|
| EC2 m8g.large | A: $0.06416/h · B: $0.0898/h | **A $0.51 · B $0.72** | $0 (auto-stopped) |
| EBS gp3 30 GB | $0.0912/GB-mo ⁽¹⁾ | $0.09 | $0.09 (24/7) |
| Public IPv4 (EIP) | $0.005/hr since Feb-2024 ⁽²⁾ | $0.12 | $0.12 (24/7) |
| CloudWatch (15 billable alarms) | $0.10/alarm-mo | $0.05 | $0.05 |
| S3 + SNS + data + Lambdas | free-tier / tiny | $0.02 | $0.02 |
| **TOTAL / day (pre-GST)** | | **A ≈ $0.79 · B ≈ $1.00** | **≈ $0.28** |
| **TOTAL / day (₹, +18% GST)** | ×85 ×1.18 | **A ≈ ₹79 · B ≈ ₹100** | **≈ ₹28** |

⁽¹⁾ AWS EBS pricing (base gp3 $0.08, Mumbai ~$0.0912). ⁽²⁾ AWS Public IPv4 charge blog (Feb 1 2024).

### Month roll-up (≈22 trading days + ≈8 off days)

| Scenario | EC2 | EIP | EBS | CW | misc | **Pre-GST/mo** | **₹/mo (+GST)** | vs $25 budget |
|---|---|---|---|---|---|---|---|---|
| **A** (rate $0.06416) | $11.3 | $3.65 | $2.74 | $1.50 | $0.50 | **$19.7** | **≈ ₹1,975** | ✅ safe |
| **B** (rate $0.0898) | $15.8 | $3.65 | $2.74 | $1.50 | $0.50 | **$24.2** | **≈ ₹2,430** | 🟠 near ceiling |
| **B + heavy manual runs (270h)** | $24.2 | $3.65 | $2.74 | $1.50 | $0.50 | **$32.6** | **≈ ₹3,270** | 🔴 **kill-switch fires** |

**The risk that matters:** with EIP on + 25 alarms + the higher instance rate, a
month with a lot of manual out-of-window runs can cross **$25 → the kill-switch
auto-stops your trading box mid-month.** Plugging the EIP + alarm leaks buys back
the headroom the original plan assumed.

---

## 4. What I recommend (corrected after verifying Terraform)

1. **Keep the EIP.** It is mandatory (no EIP → no internet → dead box). My first
   pass was wrong to call it a leak; I verified and corrected it. Update the stale
   subnet comment (`main.tf:62-66`) that still says "no EIP needed".
2. **Reconcile the digest ceiling:** make the digest's `BUDGET_INR` = the real $25
   AWS Budget (≈ ₹2,508 w/GST), so "MTD vs target %" matches what actually stops
   the box. (Safe 1-line change.)
3. **Enrich the digest** into a per-service table (EC2 / EBS / Elastic IP /
   CloudWatch split + forecast) so the daily Telegram reads like the §3 table —
   the "easy comparison anyone understands" you asked for.
4. **Confirm the digest is live + reaching Telegram** — it depends on a successful
   Terraform apply (deploy was broken until PR #1070; now fixed).
5. **CloudWatch alarms (₹150/mo):** leave them — monitoring on a trading box is
   worth more than the saving. Only prune if you find genuinely duplicate alarms.

**Net:** after verification there is **no safe money to cut** — the ~₹2,000–2,400/mo
bill is roughly correct. The real win is making the daily digest TRUSTWORTHY and
READABLE (items 2–4), not cutting cost.

---

## 5. Honest envelope (no hallucination)

- Every repo number here is `file:line`-cited; every AWS price is from an AWS page
  cited in the chat. **I could not run `aws` from this sandbox**, so I did not read
  your live bill — the daily digest does that and is the authority.
- The m8g.large Mumbai hourly rate is the one unconfirmed input; both scenarios are
  shown so you can match whichever your digest reports.
- "Guarantee" here = *the tracking mechanism exists and reads real billing data*;
  the precise rupee number is whatever Cost Explorer reports each evening — not a
  figure I can promise blind.
