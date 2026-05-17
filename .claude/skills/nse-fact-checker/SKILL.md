---
name: nse-fact-checker
description: Verify NSE F&O / Dhan / SEBI / Indian regulatory facts (rates, lot sizes, freeze limits, STT, brokerage, rate limits) against authoritative sources. Slash-command only (`/nse-fact-checker`). Trigger BEFORE adding or modifying any RATE/LIMIT/REGULATORY constant whose value comes from an external authority — typically items in the "Recommended trigger surface" section below. Skip for pure protocol constants (packet offsets, DEDUP keys) — those are covered by existing ratchet tests.
disable-model-invocation: true
allowed-tools: WebFetch, WebSearch, Read, Grep, Bash
---

# NSE F&O / Dhan Fact Checker

Real money trades on the Dhan API. SEBI changed STT in Budget 2026,
Dhan changed order rate limit in v2.3, MPP auto-conversion is effective
March 2026, static IP enforcement is effective April 2026. When
operator-facing rates change at the source, our code must follow within
one trading day — and we must KNOW which constants are stale.

This skill is a **slash-command tool** (`/nse-fact-checker`), not an
auto-trigger. Invoke explicitly when needed; do NOT block routine edits.

## Recommended trigger surface (when to /nse-fact-checker)

Use this skill BEFORE writing or modifying:

- **Rate / cost constants**: STT rates, brokerage tiers, GST rates, stamp
  duty, exchange transaction charges, SEBI fees, IPFT, demat charges.
- **Regulatory limits**: lot sizes, freeze limits, tick sizes, expiry
  rules, ASM/GSM categories, MTF leverage, position limits.
- **Dhan-specific rates**: order API rate limits (per-sec/min/hr/day),
  Data API rate limits, modification limits, free-tier caps.
- **Effective-date claims**: any "as of YYYY-MM-DD" or "effective from"
  reference about Indian regulations.
- **Dhan support replies**: drafting `docs/dhan-support/YYYY-MM-DD-*.md`
  with claims you'll send to apihelp@dhan.co.

## Do NOT use this skill for

- Binary protocol byte offsets (covered by `crates/core/src/parser/*` tests).
- ExchangeSegment numeric IDs (covered by `error_code_rule_file_crossref.rs`
  + the documented gap-at-6 ratchet).
- DEDUP key column lists (covered by `dedup_segment_meta_guard.rs`).
- Packet sizes (covered by `quality/benchmark-budgets.toml`).
- Any DH-9XX / DATA-8XX ErrorCode addition (already covered by the
  cross-ref + tag-guard ratchets — adding it here would duplicate).

The existing ratchet tests are stronger than this advisory skill for
protocol invariants. This skill exists for the *external regulatory
surface* the ratchets cannot reach.

## Authoritative source priority

When verifying any fact, search in this order. Use the FIRST bucket
that covers your claim.

| # | Source | Authoritative for |
|---|---|---|
| 1 | `docs/dhan-ref/*.md` (local) | Dhan API surface (mirrors Dhan docs at a known-good version; NOT live — may intentionally lag if a regression was rolled back) |
| 2 | `docs/dhan-support/*.md` (local) | Anything Dhan has clarified to us via ticket (e.g. #5525125, #5519522, #5610706) |
| 3a | **nseindia.com/products-services/equity-derivatives** | **NSE-defined facts**: lot sizes, freeze limits, tick sizes, contract specs, expiry calendars |
| 3b | **dhan.co/docs/** | **Dhan-defined facts**: rate limits, header names, endpoint URLs, JSON field names, WebSocket disconnect codes, IP enforcement dates |
| 4 | nsearchives.nseindia.com/content/circulars/ | NSE circular updates (margin changes, segment additions, expiry-day rules) |
| 5 | sebi.gov.in | SEBI master circulars (margin, surveillance, ASM, F&O turnover) |
| 6 | indiabudget.gov.in | Budget documents for STT rate changes |
| 7 | incometax.gov.in | Income Tax Act, STT slabs, stamp duty schedules |
| 8 | dhan.co/pricing/ | Dhan brokerage + DP charges |
| 9 | bseindia.com | SENSEX/BANKEX/BSE F&O specs |
| 10 | Zerodha Varsity / ICICIdirect / Angel One | SECONDARY ONLY — never sole source |

**Critical priority distinction:** for NSE-defined facts (lot sizes,
freeze limits, contract specs) use bucket 3a FIRST, then bucket 3b
(Dhan) only as a mirror cross-check. For Dhan-defined facts (rate
limits, header names) use bucket 3b FIRST. Mixing these gets the
priority wrong.

The CITATION URL in the metadata block MUST resolve to a domain in
buckets 1-9 above. Citing a broker blog as the sole authority for a
financial fact = REJECT.

## 5-step workflow

```
1. Identify the precise claim (rate, limit, date, behaviour).
2. Pick the smallest source-priority bucket that covers it.
3. WebFetch / WebSearch / Read the source. Capture exact value AND
   "effective from" / "as of" / circular number / page reference.
4. Build a comparison table:
   | Claim | Source value | Status | Citation URL |
5. Either:
   (a) MATCH → annotate the constant with the 5-line metadata block
       below (see "Recommended output format").
   (b) MISMATCH → present BOTH values + suspected regulatory event.
       DO NOT auto-apply. Ask operator for explicit approval.
   (c) NO SOURCE in 3 searches → mark UNVERIFIABLE in code; ask
       operator to confirm or supply.
```

## Status codes

- ✅ **VERIFIED** — exact match against authoritative source. Cite URL + date.
- ⚠️ **STALE** — was correct historically; a newer regulation supersedes it. Flag for update.
- ❌ **INCORRECT** — contradicts authoritative source. Fix BEFORE next commit.
- ❓ **UNVERIFIABLE** — no primary source found within 3 searches. Mark explicitly in code.

## Recommended output format (NEW constants on the trigger surface)

For NEW rate / cost / regulatory constants added on the trigger surface,
use this 5-line metadata block as a Rust doc comment:

```rust
/// RATE: 10 orders per second per dhanClientId
/// SOURCE: Dhan API v2 — Annexure & Enums §4 (rate limits)
/// CITATION: https://dhanhq.co/docs/v2/annexure/#rate-limits + docs/dhan-ref/08-annexure-enums.md
/// VERIFIED: 2026-05-17 by nse-fact-checker (Dhan support round preceding ticket #5610706)
/// REVIEW BY: 2026-08-17 (quarterly re-check)
pub const ORDER_API_RATE_LIMIT_PER_SEC: u32 = 10;
```

This is a **recommended template, not a hard requirement.** Existing
constants in `crates/common/src/constants.rs` use 1-2 line free-form
rationale comments — that is OK and should NOT be retroactively
churned. The 5-line block applies to NEW rate / cost / regulatory
constants added going forward, AND when updating an existing one
where the value changes due to a regulation update.

For protocol constants (packet offsets, DEDUP keys, packet sizes,
WebSocket request codes) the existing 1-2 line style remains correct.

## Hard rules

1. **NEVER fabricate a Dhan support ticket number.** Tickets live in
   `docs/dhan-support/`; if no ticket exists for the claim, say so.
2. **NEVER cite a source dated before the last known regulatory change**
   for that fact. (Example: an STT citation from 2024 is STALE if
   Budget 2026 changed STT.)
3. **NEVER claim a value is "current" without showing the verification date.**
4. **If two sources conflict, present BOTH to the operator and ask.**
5. **If no primary source found within 3 searches, mark UNVERIFIABLE
   and escalate to operator. Do NOT guess.**
6. **The CITATION URL MUST resolve to a domain in source priority
   buckets 1-9.** Broker blogs (Zerodha Varsity, etc.) are bucket-10
   secondary only and never the sole authority for a financial fact.

## What this prevents

- Hallucinated rate limits.
- STT rate drift across budget years.
- Stale lot-size constants when NSE revises mid-quarter.
- Citing broker blogs as authority when SEBI circulars exist.
- Real-money trades made against rates last verified months ago.

## What this skill does NOT cover

- Pure-code refactoring (no external factual claim, no trigger).
- Internal tickvault architecture decisions (the operator-charter + 11 rule files cover those).
- Generic Rust patterns (the rust-code.md rule covers those).
- Binary protocol invariants (existing ratchet tests are stronger).

This skill exists for the EXTERNAL regulatory surface (NSE, BSE, SEBI,
Dhan, Income Tax, Budget). Pure-code is covered by clippy + the 14
ratchet meta-guards.
