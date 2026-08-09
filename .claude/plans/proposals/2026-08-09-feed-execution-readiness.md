# Feed execution readiness — the fuel line

**Status:** NOT A NEW PLAN. This is a readiness + correction note for an
**already-APPROVED** plan.
**Date:** 2026-08-09
**The approved artifact:** `.claude/plans/active-plan-truedata-feed.md` —
**Status: APPROVED**, Date 2026-07-24, *"Approved by: Parthiban (operator) —
approved 2026-07-24 in-session"*. **0 of 6 items done.**
**Scope authority:** `.claude/rules/project/truedata-feed-scope-2026-07-24.md`

---

## The headline

**You do not need a feed plan written. You approved one 16 days ago and it was
never built.**

| | |
|---|---|
| Plan exists | ✅ `active-plan-truedata-feed.md` |
| Operator-approved | ✅ **2026-07-24** |
| Scope lock in place | ✅ `truedata-feed-scope-2026-07-24.md` (§0 quotes, §9/§10 evidence) |
| Implementation authorized | ✅ the lock's own gate — *"PRs B–F may only start once that plan file's Status flips to APPROVED"* — **it is APPROVED** |
| Items built | ❌ **0 of 6** |

So this is an **execution gap, not a planning gap.** Nothing is waiting on a
decision from you except a commercial trial key.

## The 6 approved items, and what each really is

| # | Item | Size | Blocked on |
|---|---|---|---|
| **PR-A** | Scope-lock + plan + 2 pointer lines | docs | ✅ effectively done (the lock is committed) |
| **PR-B** | `Feed::Truedata` + config + `WsType` + namespace bits 59/58 | small, 4 files | nothing — **can start today** |
| **PR-C** | Native WSS client + 90-byte binary parser + auth + SSM 1-session lock + **session-0 probe gate** | **large** | nothing to write; the probe needs the sandbox key to RUN |
| **PR-D** | Wire into WAL→ring→spill→DLQ + **REBUILD the tick→TF aggregator** (hard-deleted 2026-07-17) + seq-gap detector | **largest** | see the correction below |
| **PR-E** | **REBUILD the tick writer** (`tick_persistence.rs` + `DEDUP_KEY_TICKS`, both deleted 2026-07-17) | large | see the correction below |
| **PR-F** | Observability + trial runbook | medium | nothing |

**Honest sizing:** PR-C, PR-D and PR-E each rebuild something that was
**deliberately deleted** in the 2026-07-17 dead-WS sweep. That is recoverable from
git history but it is not a small diff — the plan says so itself for the aggregator
(*"HARD-DELETED 2026-07-17, recover from git pre-sweep; NOT a revive"*).

## ⚠️ Correction the approved plan needs (do not silently work around it)

PR-D commits to *"a **SEPARATE 1s lane** (1s is NOT in TfIndex)"* and PR-E to
*"**new `candles_1s` table**"*.

**Both premises are factually wrong against the code today.** Verified by
enum-variant scan of `crates/trading/src/candles/tf_index.rs`:

```
S1 = 5,     <- 1-second frame, ALREADY IN TfIndex
```

`S1` was added on **2026-07-21** by the C3 second-scale directive — **three days
BEFORE this plan was approved on 2026-07-24.** All 16 second-scale frames (S1–S15,
S30) are already wired through `ALL`, `from_ordinal`, `table_name`,
`seconds_per_bucket` and `display_name`. `S1.table_name()` resolves today.

**Consequence:** the separate-1s-lane and the `candles_1s` table are **probably
redundant**. PR-D/PR-E should open with *"verify S1 already folds"* rather than
building a parallel lane. Left as a FLAG, not an edit — that plan is the operator's
approved artifact and re-scoping its items needs its own dated note.

## The two real blockers

| # | Blocker | Who | Note |
|---|---|---|---|
| **1** | **A TrueData trial key** (sandbox `wstest.truedata.in:8086`, then the assigned prod port) | **Operator — commercial** | Nothing can be probed without it |
| **2** | Implementation time for PR-B..PR-F | Me | Authorized; not started |

### What to confirm with TrueData in writing before the trial (scope-lock §9.9)

| Item | Why |
|---|---|
| **"Live data (Streaming Ticks)"**, NOT a bars-only plan | A bars plan cannot feed tick-derived timeframes at all |
| Segments: **NSE EQ + NSE Indices + NSE F&O** | Per-account entitlement |
| Symbol cap **≥ 500** | The plan's target set |
| **Bid/Ask L1 ON** | Needed for worst-case-fill lot sizing |
| The **assigned prod port** | Never hardcoded; SSM-supplied |
| **Sandbox access** | Required for the §9.8 day-0 probe |

## ⚠️ The granularity truth — read before paying

Your own reference pack already settled this, and it is the single most important
expectation to set:

> **No Indian vendor redistributes true tick-by-tick.** NSE states TBT *"is
> available only at NSE co-lo servers … not available at TAP Server or through
> DotEx for further broadcast"* (`catalog-truedata.md:39`). TrueData's own feature
> grid says **"L1 data @1sec frequency"** (`01-overview.md:46`).

So **"never miss a tick" means "never lose a message we RECEIVE"** — never "observe
every trade". Intra-second trades are conflated **upstream, before they reach us**,
and no architecture on our side can recover them.

What we CAN guarantee: capture-completeness of every received message
(WAL-before-broadcast → 200,000-seal ring → NDJSON spill → DLQ) **plus** an
independent vendor-side loss proof via the per-symbol **Sequence No** field — a gap
fires `TD-GAP-01` naming the missing range.

The vendor PDF asserts tick-by-tick; the NSE structural evidence says conflated.
**This note takes no side** — the §9.8 day-0 probe MEASURES msgs/sec/symbol at open
and the measured number is what gets reported, never asserted in advance.

## Also still open, from §10.6 — settled only by the live probe

| # | Unknown | Risk if guessed |
|---|---|---|
| 1 | How the client SELECTS binary vs JSON (both documented, the switch never stated) | Wrong parser entirely |
| 2 | Timestamp precision (ms vs whole seconds) | Dedup key correctness |
| 3 | Seq-No scope / reset / wrap | False `TD-GAP-01` storms, or masked real loss |
| 4 | SymbolID stability across sessions | Identity corruption on reconnect |
| 5 | Real granularity (see above) | The whole "never miss a tick" claim |

**Endianness is SETTLED** — little-endian, confirmed from the vendor's own client
(`utils.py` unpack format strings, scope-lock §11.1). That was the highest-risk
unknown (a wrong guess silently garbles every price) and it is closed.

## Recommended order

| # | Step | Who | Why first |
|---|---|---|---|
| 1 | **Merge PR #1730** | You | The box must be alive before any feed work is testable |
| 2 | Request the trial key + the 6 written confirmations | You | Longest lead time — start it now, in parallel |
| 3 | **PR-B** (`Feed::Truedata` + config, default-OFF) | Me | Zero risk, unblocks C–F, no key needed |
| 4 | PR-C parser + probe harness | Me | Written offline; RUN when the key lands |
| 5 | Day-0 sandbox probe | Both | Settles the 5 unknowns with measurements |
| 6 | PR-D / PR-E / PR-F | Me | Shaped by what the probe actually returns |

**Step 3 can start the moment you say go — it needs no key and no new approval.**

## Honest envelope

> The plan and scope lock are real, committed, and operator-approved; PR-B is
> genuinely startable today with zero new authorization.
> **NOT claimed:** (a) that a feed will be live soon — items C/D/E rebuild three
> subsystems deleted on 2026-07-17 and that is substantial work, not a switch;
> (b) that TrueData will deliver tick-by-tick — the evidence says ~1-second
> conflated and the probe is the arbiter; (c) that the trial key is obtainable or
> priced acceptably — that is entirely a commercial matter outside this repo;
> (d) that PR-D/PR-E as written are correct — this note FLAGS their 1s-lane premise
> as contradicted by the code and does not act on it.

## Auto-driver explanation

> Sir, you asked me to plan the fuel line. Good news: **you already approved the
> fuel-line plan on 24th July** — the drawings are signed, filed, and nobody ever
> started building. So there is nothing left to decide except one thing that only
> you can do: **ring the supplier and get the trial connection.** That takes the
> longest, so start it today.
>
> Meanwhile I can bolt on the first piece — the connector plate — with no key and
> no new permission needed.
>
> Two honest warnings. One: three of the six pieces rebuild parts we deliberately
> threw away in July, so it is real work, not a switch. Two: **no supplier in India
> is allowed to sell you every single trade** — the exchange only gives that inside
> its own building. They all send a snapshot about once a second. So "never miss a
> tick" honestly means "never lose what we're sent" — anyone promising you every
> trade is selling you something.
