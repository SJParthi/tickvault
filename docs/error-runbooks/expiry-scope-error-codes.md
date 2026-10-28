# Expiry Scope — Error Code (EXPIRY-SCOPE-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` > this file.
> **Owner request (2026-10-28):** "go ahead and add the daily expiry check" —
> proof, every trading day, that only the CURRENT expiry of each underlying is
> ever subscribed.
> **Plan item:** EX1 in `.claude/plans/active-plan-audit-2026-10-20.md`.
> **Companion code:** `crates/app/src/expiry_scope_check.rs` (both layers),
> `crates/storage/src/expiry_scope_audit_persistence.rs` (the
> `expiry_scope_audit` writer), and
> `crates/common/src/error_code.rs::ExpiryScope01OffExpiry`.

## The two layers

| Layer | When | What it checks | On a finding |
|---|---|---|---|
| Subscribe time | every contract attach attempt, before the selection is named, mapped or dialled | every selected option is on or after today AND on its underlying's nearest real expiry (smallest expiry with at least 4 legs in today's contract file), recomputed by a pass separate from the selector | the contract is REFUSED (not subscribed), counted on `tv_expiry_scope_subscribe_refused_total`, one `EXPIRY-SCOPE-01` line with `stage = "subscribe"` and a sample name |
| Daily | 15:50 IST on trading days, own task | today's distinct option contracts in `candles_1m` (feed `dhan`, `NSE_FNO` / `BSE_FNO`), expiry from the stored name or the contract file | one `expiry_scope_audit` row; `EXPIRY-SCOPE-01` with `stage = "daily"` on a violation |

## Daily verdicts (`expiry_scope_audit.verdict` / `reason`)

| verdict | reason | meaning |
|---|---|---|
| `clean` | `all_current_expiry` | every stored contract was judged and is on its current expiry |
| `violation` | `off_expiry` / `expired` / `expired_and_off_expiry` | at least one stored contract is not current (reported even if other rows could not be judged) |
| `partial` | `artifact_missing` | today's contract file could not be read |
| `partial` | `read_failed` | the `candles_1m` read failed 3 times, 120 s apart |
| `partial` | `row_cap_hit` | more than 100,000 distinct rows came back |
| `partial` | `no_rows` | nothing to judge; a zero proves nothing |
| `partial` | `unverifiable_rows` | an unknown underlying, or no usable name and no file row |
| `partial` | `label_mismatch` | a stored name disagrees with the contract file (or two names for one contract) |
| `partial` | `malformed_labels` | a stored name does not parse (the contract file still identified the contract) |

`unlabelled` (a contract with a candle that carries no name) is counted but
does not spoil a clean day when the contract file still identifies it.

## EXPIRY-SCOPE-01 — a contract outside its underlying's current expiry

**Severity:** High. **Auto-triage:** No (a scope verdict the operator judges).
**Delivery:** log only. No CloudWatch filter, no EMF name, no alarm, no
Telegram (`dhan-rest-only-noise-lock-2026-07-14.md` §3).

**Triage:**
1. `select * from expiry_scope_audit order by ts desc limit 5` — the counts
   and `sample_offender`.
2. `stage = "subscribe"`: the selector picked a contract the check rejects.
   Read the attach's `contract universe resolved` line (`refused_off_expiry`)
   and compare the sample with today's contract file
   (`data/instrument-cache/dhan-contracts-<date>.json`, fields `u`, `e`). A
   refusal means the selector, the file or the vendor's listing changed; the
   refused contract was NOT subscribed.
3. `stage = "daily"` with `expired`: a candle of today belongs to a contract
   whose expiry is past. The subscribe layer refuses such a contract, so look
   at how the candle was written: the boot log of any restart today (frame
   replay) and the socket logs for that id.
4. `stage = "daily"` with `off_expiry` and `subscribe_refused = 0`: the
   subscribe layer passed it, so the candle came from a contract that was not
   in today's selection (a backup copy, a replay, or a vendor relisting).
   Query `select distinct contract from candles_1m where ts in today() and
   contract = '<sample>'` and the socket logs for that id.

## The naming gap found on 2026-10-28

One `candles_1m` row of 2026-10-28 (HAVELLS 23Nov2026 1260 CE, id 114816)
carried `contract = NULL`. Cause: the attach retries through the morning and
REPLACED the name table on every attempt from that attempt's selection only.
A contract an earlier attempt subscribed (the socket keeps it) but the latest
attempt no longer selected (the at-the-money window narrows as more stocks
price) fell past the 25,000-name cap. Fixed the same day: the attach now names
everything selected earlier today as a second tier
(`contract_underlying_map::labels_from_artifact_in_tiers`,
`earlier_selected_today`). The after-close name repair
(`contract_name_repair_boot`) also fills such rows from the day's files.
