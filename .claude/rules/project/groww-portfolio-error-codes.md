# Groww Portfolio Snapshot Surface — Error Codes (GROWW-PORT-01..04)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `groww-second-feed-scope-2026-06-19.md` §39 (the 2026-07-14 operator
> authorization of the Groww order-side build; §39.3 area-collision contract
> — `GROWW-PORT-` is the Portfolio area's reserved family) >
> `no-rest-except-live-feed-2026-06-27.md` §10 (`portfolio_read` /
> `margin_read` are the Gate-1 entries) >
> `groww-shared-token-minter-2026-07-02.md` (token READ-ONLY) > this file.
> **Status:** CONTRACT STUBS (item 6c.1) — variants + this runbook land
> together; emit sites land in 6c.2/6c.3/6c.4. Until those merge, ZERO
> production code emits these codes.
> **Companion code (planned):** `crates/common/src/portfolio_types.rs`
> (canonical types + the fail-closed snapshot handle),
> `crates/trading/src/oms/groww/portfolio.rs` (probe + tiers + fetch, feature
> `groww_orders`), `crates/storage/src/broker_portfolio_persistence.rs`
> (the snapshot/holding/margin tables + `position_recon_audit`),
> `crates/common/src/error_code.rs::ErrorCode::{GrowwPort01SnapshotDegraded,
> GrowwPort02PersistFailed, GrowwPort03ReconDivergence,
> GrowwPort04ForeignPosition}`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires every `GrowwPort0*` variant mentioned verbatim — `GROWW-PORT-01`,
> `GROWW-PORT-02`, `GROWW-PORT-03`, `GROWW-PORT-04` appear below.

---

## §0. Why these codes exist (the portfolio snapshot surface)

The Groww account is SHARED with the co-tenant program (BruteX) and, until
live trading is authorized, this system places ZERO orders (`dry_run = true`).
The portfolio subsystem is a READ-ONLY snapshot surface: scheduled tiers (T1
intraday / T2 15:20 / T3 15:25 orphan check / T4 15:35 authoritative book)
pull positions + margin (+ holdings at T4) into a fail-closed RAM snapshot
(`portfolio_types.rs` — `None` ∨ not-Complete ∨ stale ⇒ no consumer decision)
and the feed-in-key QuestDB tables. A three-book ledger (B1 our RAM decision
book, B2 the broker snapshot book, B3 our fill-event book) classifies every
residual D1–D13; evidence-only attribution marks everything we did not place
FOREIGN — in dry-run the ENTIRE broker book is foreign by identity, the
daily co-tenant-activity canary. Every failure class below is a DEGRADE —
the live tick path, candles, and trading are never touched; every emit
carries `feed = "groww"` (+ stage/kind). All four codes route HERE.

## §1. GROWW-PORT-01 — portfolio snapshot fetch degraded

**Severity:** High. **Auto-triage safe:** Yes (the next tier/cycle
re-attempts — the operator inspects, never manually re-fetches first).

**Trigger:** `ErrorCode::GrowwPort01SnapshotDegraded`; the `stage` field
(the SPOT1M-01 taxonomy): `cycle_failed` (coalesced ONCE per failed cycle —
transport / non-2xx / envelope-FAILURE / parse) · `escalation` (the EDGE: 3
consecutive fully-failed cycles, PERSIST-GATED — fetch-ok-but-lost is not
"ok"; ONE typed HIGH `GrowwPortfolioSnapshotDegraded` per episode, re-armed
only by a full fetch+persist success; recovery = one Info
`GrowwPortfolioSnapshotRecovered`) · `token_read` (shared-minter SSM read
failed — re-read ≥60s, NEVER minted) · `client_build` (HTTP-CLIENT-01
class) · `task_respawn` (supervised scheduler died) · `resolve_failed` (a
wire row did not resolve via the Groww master — row excluded, snapshot NOT
Complete) · `schema_drift` (edge-latched unparsed-field-count delta vs the
probe baseline) · `rate_limited` (429 counted + one bounded backoff, never
out-polled) · `blind_tier` (a one-shot tier still blind AFTER its 3-attempt
+30s/+60s in-tier ladder — DECLARED, never all-clear; incl. the boot-time
back-check declarations for tiers a dead process skipped).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-PORT-01`; the payload
   carries `stage`, tier, per-leg outcomes and a bounded secret-redacted
   sample failure (status + redacted URL + ≤300-char body).
2. `tv_groww_portfolio_fetch_total{leg,outcome}` names the dominant class;
   cross-check the Groww REST 1m legs (SPOT1M-01, `feed="groww"`) — shared
   token + pooled rate bucket, so they degrade together. A ~06:00 IST stale
   token is the daily minter reset (self-heals); sustained `token_read` =
   the minter Lambda is dead (its own ~10-min alert ladder pages).

**Honest envelope:** bounded, loud attempts per tier; the fetcher cannot
force the broker to serve the book. A missed cycle leaves the snapshot stale
— consumers fail closed on age (`is_fresh`), never a fabricated row.
SUCCESS+empty parses `ok_empty`, never conflated with failure (Rule 11); a
cold-boot empty+margin-quiet cycle labels `ok_empty_cold_boot`, never
"confirmed flat".

## §2. GROWW-PORT-02 — portfolio persist failed

**Severity:** High. **Auto-triage safe:** Yes (best-effort persist; the fetch
loop continues; re-appends are DEDUP-idempotent).

**Trigger:** `ErrorCode::GrowwPort02PersistFailed`, `stage` field:
`ensure_client_build` / `ensure_ddl` (HTTP-CLIENT-01-class consequence: the
first ILP write may auto-create a table WITHOUT DEDUP UPSERT KEYS — a
duplicate-row window until a later ensure succeeds) · `append` · `flush`
(the ILP-over-HTTP per-request server ACK refused — the 2026-07-05
fire-and-forget lesson; pending rows DISCARDED, the poisoned-buffer defense)
· `audit_*` (the `rest_fetch_audit` / `position_recon_audit` forensic legs —
never affects the fetch loop). The RAM snapshot STILL publishes, and the T4
daily-digest Telegram is PERSIST-INDEPENDENT (RAM-computed) — but the failed
cycle counts toward the GROWW-PORT-01 escalation edge (persist-gated).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — the `stage` names the failing leg;
   `tv_groww_portfolio_persist_errors_total{stage}` non-zero → QuestDB
   ILP/HTTP degraded; `make doctor` (cross-check BOOT-01/BOOT-02 +
   WAL-SUSPEND-01 — the sibling REST-leg writers share the target).
2. `mcp__tickvault-logs__questdb_sql "select count(*) from
   broker_portfolio_snapshot where ts > dateadd('h', -1, now())"` — rows
   land again after recovery. A LOST yesterday-T4 book makes the next day's
   compare BLIND-baseline (§3) — never "no drift".

**Honest envelope:** forensic/reference tables — an outage loses rows for
the window only; never tick capture, candles, or the RAM snapshot. (The PR-L
`flatten_leg_audit` is an IDEMPOTENCY LEDGER, not forensics —
discard-pending FORBIDDEN there; NOT covered by this code.)

## §3. GROWW-PORT-03 — reconciliation residual confirmed

**Severity:** High. **Auto-triage safe:** **NO — severity-independent
override arm (FUTIDX-02 precedent: data-comparability divergence is never
auto-actioned).** The operator judges whether the broker book, our book, or
the co-tenant explains the residual — neither is ground truth (§37 doctrine).

**Trigger:** `ErrorCode::GrowwPort03ReconDivergence` — the three-book ledger
confirmed a D1/D4/D5/D8-class residual (integer-paise/qty identities vs
`tolerance_paise`, default 0 = exact) on 2 CONSECUTIVE snapshots for a
`(security_id, exchange_segment, product)` key. `kind` carries the D1..D13
slug; each finding is a `position_recon_audit` row (`kind` + `outcome`
IN-KEY so transition rows both survive) with `blame` ∈ {ours, broker,
co_tenant, indeterminate} — `_ => indeterminate` fail-closed floor, never
blank. D13 `broker_rms_squareoff` (MIS + [15:15, 15:30) IST + no B3 fill) is
blame=broker at Info, never a page. A MISSING yesterday-T4 baseline makes
the day-over-day compare BLIND-baseline — declared, never "no drift"
(Rule 11).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — the payload names the key, `kind`,
   residuals in paise/qty, and `blame`.
2. `mcp__tickvault-logs__questdb_sql "select kind, blame, count(*) from
   position_recon_audit where trading_date_ist = today() group by kind,
   blame"` — track the trend vs prior days, not the absolute (the
   CROSS-VERIFY-1M-01 discipline).
3. The typed HIGH `GrowwPortfolioReconDivergence` Telegram names the
   instrument(s) and carries the shared `recon_incident_id` (one page per
   incident-id per day per session). Eyeball the named instrument(s) in the
   broker app before any manual order in them.

## §4. GROWW-PORT-04 — foreign position detected

**Severity:** High. **Auto-triage safe:** Yes — visibility only: the page IS
the dry-run action; **NOTHING is ever auto-actioned by code. Never-auto-exit
is a source-scan-ratcheted invariant** (any future flatten enumerates ONLY the
ours-attributed sub-book — `test_never_auto_exit_foreign_source_scan`).

**Trigger:** `ErrorCode::GrowwPort04ForeignPosition` — attribution by OUR
fill evidence only (`ours = explained_qty`; everything above is
co-tenant/manual). The `kind` field: `new_key` / `sign_flip` ⇒ IMMEDIATE
typed `GrowwForeignPositionDetected` page (hash domain `(instrument_key,
product, sign(net_qty), qty_bucket)`, re-armed at IST midnight) ·
`qty_bucket_change` / `set_shrunk` ⇒ daily digest only (the fatigue split) ·
`unqueried_segment_active` (nonzero margin `commodity_*` usage with zero
commodity position coverage — the N1 shared-account blind-spot tripwire,
zero-request, every cycle) ⇒ one page per day. In dry-run
`explained_qty ≡ 0`, so the whole broker book is foreign — expected.

**Triage:**
1. `tv_groww_portfolio_foreign_positions` (gauge) + the day's
   `position_recon_audit` foreign rows (`first_seen_date` + delta) name the
   set. No dry-run action beyond eyeballing the broker app; margin figures
   are ACCOUNT-POOLED — never "our funds".
2. A foreign row on an instrument WE hold (live era) escalates — any future
   exit clamps to `min(|ours|, |broker net|)`; opposite-sign overlap makes
   account-flat unachievable by us (`flat_blocked_by_cotenant`, PR-L scope).

## §5. Delivery boundary (honest — no false-OK)

All four codes are **log-sink-only today**: NO `error_code_alerts` map entry
in `deploy/aws/terraform/error-code-alarms.tf` and NO mention in
`observability-architecture.md`'s paging list. The operator pages are the
typed Telegram edges — `GrowwPortfolioSnapshotDegraded` (persist-gated
3-cycle escalation) + `GrowwPortfolioSnapshotRecovered`,
`GrowwPortfolioReconDivergence`, `GrowwForeignPositionDetected`, and the
Info `GrowwPortfolioDailyDigest` (T4, RAM-computed, persist-independent) —
the SCOREBOARD-01 precedent; the coded `error!` lines are the forensic WHY.
A CloudWatch log-filter alarm is a **flagged follow-up** (one map entry +
doc paragraph + cost note — the FEED-REJECT-01 precedent).

**Telemetry (`tv_groww_portfolio_*` family):**
`tv_groww_portfolio_fetch_total{leg,outcome}` · `_rate_limited_total` ·
`_persist_errors_total{stage}` · `_rows_discarded_total` ·
`_task_respawn_total{reason}` · `_blind_tiers_total{tier}` ·
`_recon_findings_total{kind}` · `_foreign_positions` (gauge) ·
`_snapshot_age_secs` (gauge — the staleness consumers gate on) ·
`_close_to_data_ms` (histogram)

## §6. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `GrowwPort0*` variant)
- `crates/common/src/portfolio_types.rs`
- `crates/trading/src/oms/groww/portfolio.rs` (+ any recon sibling under
  `oms/groww/`) or `crates/storage/src/broker_portfolio_persistence.rs`
- Any file containing `GROWW-PORT-`, `GrowwPort0`, `portfolio_types`,
  `PortfolioSnapshot`, `broker_portfolio_snapshot`, `position_recon_audit`,
  `tv_groww_portfolio_`, or any path under `oms/groww/portfolio`
