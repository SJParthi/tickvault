# Groww Margin Area — Error Codes (GROWW-MARG-01..05)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `groww-second-feed-scope-2026-06-19.md` §39 +
> `no-rest-except-live-feed-2026-06-27.md` §10 (the 4-gate live-fire
> lattice the Groww order-side lives behind; `margin_read` is the Gate-1
> entry) > `.claude/rules/dhan/funds-margin.md` (the Dhan-side pooled-
> account + fail-closed margin-gate doctrine this area mirrors) >
> `groww-shared-token-minter-2026-07-02.md` (token READ-ONLY) > this file.
> **Operator context:** the 2026-07-14 Groww order-side build authorization
> (§39.0 verbatim: *"confirm — apply the Groww order scope-unlock PR-0.
> Build only, behind the OFF switch, no live orders"*) + the 2026-07-15
> coordinator fan-out mandate (contracts-first stubs — relayed intent,
> labeled as such).
> **Companion code (FUTURE — lands in the Margin area code PR):**
> `crates/trading/src/oms/groww/margin.rs` (feature `groww_orders`, per
> §39.3 of the groww-second-feed scope lock). NO production code emits
> these codes today.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `GrowwMarg0*` variant verbatim —
> `GrowwMarg01FetchDegraded`, `GrowwMarg02PersistFailed`,
> `GrowwMarg03SnapshotStaleGateClosed`,
> `GrowwMarg04EntryRejectedInsufficient`, `GrowwMarg05CalcDivergence` and
> `GROWW-MARG-01`, `GROWW-MARG-02`, `GROWW-MARG-03`, `GROWW-MARG-04`,
> `GROWW-MARG-05` appear below.

---

## §0. Why these codes exist (contracts-first stubs)

The Groww Margin area (user-margin detail + the margin calculator + the
pre-entry margin gate, §39.3) lands as its own area code PR on
`crates/trading/src/oms/groww/margin.rs`. Per the INSTR-FETCH Sub-PR #9 /
GROWW-SCALE contracts-first sequencing, the five `ErrorCode` variants +
this runbook land FIRST so the area PR compiles against stable identifiers
with zero shared-file contention.

**Honest envelope (§F, stated plainly):** this PR ships ONLY contract
stubs — ZERO production code emits these codes until the Margin area code
PR lands. The gate knobs referenced below — `buffer_pct`, `floor_paise`,
`stale_secs` — are the AREA PR's config design values (deliberately NOT
added to `config.rs` in this PR), recorded here so the contract is
reviewable now. The gate doctrine mirrors the Dhan-side precedent
(`funds-margin.md` rule 10): fail CLOSED for entries, NEVER gate exits.

## §1. GROWW-MARG-01 — margin fetch degraded

**Severity:** High. **Auto-triage safe:** Yes (the degrade already
happened; the next poll retries automatically).

**Trigger:** `ErrorCode::GrowwMarg01FetchDegraded` — the
`/v1/margins/detail/user` fetch leg degraded: transport error, non-2xx,
unparsable body, or no shared-minter token at fire time. The next poll
retries; the read is market-hours-gated + config-gated DEFAULT-OFF per
the §39.2 lattice.

**Triage (stub — counters land with the area PR):**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-MARG-01`; the payload
   names the failing leg (transport / parse / token) with a bounded
   secret-redacted sample.
2. `tv_groww_margin_fetch_total{outcome}` (area-PR telemetry family
   `tv_groww_margin_*`) rates name the dominant class; cross-check the
   shared token + the sibling Groww REST legs.
3. A sustained degrade ages the margin snapshot toward the
   GROWW-MARG-03 stale gate — the fail-closed consequence is designed.

## §2. GROWW-MARG-02 — margin snapshot persist failed

**Severity:** High. **Auto-triage safe:** Yes (best-effort persist; the
next cycle's re-append is DEDUP-idempotent).

**Trigger:** `ErrorCode::GrowwMarg02PersistFailed` — the margin snapshot
persist leg failed (ensure-DDL / append / flush — the ILP-over-HTTP
per-request-ACK house pattern).

**Triage (stub):**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-MARG-02`; the `stage`
   names the failing leg.
2. `tv_groww_margin_persist_errors_total{stage}` (area-PR counter)
   non-zero → QuestDB ILP/HTTP degraded; run `make doctor` (cross-check
   BOOT-01/BOOT-02).
3. The RAM snapshot (the gate's input) is persist-independent; only the
   forensic rows for the outage window are missing.

## §3. GROWW-MARG-03 — margin snapshot stale, gate fails CLOSED

**Severity:** High. **Auto-triage safe:** Yes (the fail-closed refusal is
the design working — the page is visibility, never an action request).

**Trigger:** `ErrorCode::GrowwMarg03SnapshotStaleGateClosed` — the margin
snapshot is older than `stale_secs` (area-PR config knob), so the margin
gate fails CLOSED for entries: no entry may be authorized on stale margin
data. EXITS ARE NEVER MARGIN-GATED (the `funds-margin.md` rule 10
doctrine — an exit must always be placeable).

**Triage (stub):**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-MARG-03`; the payload
   carries the snapshot age vs `stale_secs`.
2. The root cause is upstream — cross-check GROWW-MARG-01 (fetch degrade)
   and the shared token; entries resume automatically once a fresh
   snapshot lands.

## §4. GROWW-MARG-04 — entry refused: insufficient margin

**Severity:** High. **Auto-triage safe:** **No — severity-independent
override arm** in `is_auto_triage_safe()` (the FUTIDX-02 override
precedent): spend authorization on a POOLED shared account is operator
territory — the `funds-margin.md` rule 8 shared-account doctrine (the
BruteX co-tenant shares the balance; `insufficient == 0` alone is never
spend authorization, and a refusal is never auto-overridden).

**Trigger:** `ErrorCode::GrowwMarg04EntryRejectedInsufficient` — an entry
was refused because the available margin, after applying `buffer_pct` +
`floor_paise` (area-PR config knobs), is insufficient for the sized
order. The refusal is the fail-closed design; the page tells the operator
WHY an intended entry did not happen.

**Triage (stub):**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-MARG-04`; the payload
   carries the required vs available figures (paise integers).
2. Margin figures are ACCOUNT-POOLED — the co-tenant may have consumed
   headroom between polls (irreducible TOCTOU; the broker is the final
   arbiter). The operator decides whether to fund, resize, or stand down
   — never auto-triage.

## §5. GROWW-MARG-05 — calculator vs user-margin divergence

**Severity:** Medium. **Auto-triage safe:** Yes (visibility trend signal;
the next poll re-measures).

**Trigger:** `ErrorCode::GrowwMarg05CalcDivergence` — the margin
calculator's figure diverged from the user-margin detail beyond the
area-PR tolerance. Neither side is ground truth (the §37 doctrine); the
divergence is counted and trended, never silently normalized.

**Triage (stub):**
1. `mcp__tickvault-logs__tail_errors` — find `GROWW-MARG-05`; the payload
   carries both figures + the tolerance.
2. Track the trend (a stable small divergence is broker-side rounding /
   timing skew; a spike is a parse or entitlement drift — cross-check
   GROWW-MARG-01).

## §6. Honest envelope + delivery boundary

All five codes are **log-sink-only**: NO `error_code_alerts` map entry, no
CloudWatch filter, no Telegram emit — the typed Telegram surface lands
with the area code PRs, and any Groww page class is governed by the §39
live-fire lattice (any Dhan-side page class by
`dhan-rest-only-noise-lock-2026-07-14.md`). The telemetry family
`tv_groww_margin_*` is the area PR's counter namespace. NOT claimed: live
Groww margin API behaviour (field semantics, calculator parity —
UNVERIFIED-LIVE until the first authorized session); any order fire
(Gates 1–4 all closed; `dry_run` stays true); that the pooled balance is
"our funds" (it never is — shared account).

## §7. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `GrowwMarg0*` variant)
- `crates/trading/src/oms/groww/margin.rs` (the future area file)
- Any file containing `GROWW-MARG-01`, `GROWW-MARG-02`, `GROWW-MARG-03`,
  `GROWW-MARG-04`, `GROWW-MARG-05`, `GrowwMarg01FetchDegraded`,
  `GrowwMarg02PersistFailed`, `GrowwMarg03SnapshotStaleGateClosed`,
  `GrowwMarg04EntryRejectedInsufficient`, `GrowwMarg05CalcDivergence`, or
  `tv_groww_margin_`
