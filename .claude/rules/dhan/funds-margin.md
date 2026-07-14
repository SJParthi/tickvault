# Dhan Funds & Margin Enforcement

> **Ground truth:** `docs/dhan-ref/13-funds-margin.md`
> **Scope:** Any file touching the funds/margin REST wrappers (fund limit, single-margin
> calculator, multi-margin calculator) or the pre-trade margin gate.
> **Cross-reference:** `docs/dhan-ref/08-annexure-enums.md` (ExchangeSegment, rate limits),
> `docs/dhan-ref/07-orders.md` (order placement — the margin gate's downstream consumer),
> `.claude/rules/project/dhan-rest-only-noise-lock-2026-07-14.md` (the Dhan alert-surface lock
> the gate must respect when it grows paging).

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any funds/margin
   handler, margin struct, or the pre-trade margin gate: `Read docs/dhan-ref/13-funds-margin.md`.

2. **Three endpoints — exact URLs and methods.**
   - Single-order margin: `POST /v2/margincalculator`
   - Multi-order margin: `POST /v2/margincalculator/multi`
   - Fund limit: `GET /v2/fundlimit`
   All three carry the `access-token` header. The funds APIs carry NO `client-id` header
   requirement (unlike Option Chain / Market Quote) — our shared client sends both headers
   anyway, which is harmless.

3. **The fundlimit `availabelBalance` typo is Dhan's — NEVER "fix" it.** The fund-limit
   response spells it `availabelBalance` (missing 'l'); our struct maps it via
   `#[serde(rename = "availabelBalance")]`. The SINGLE-margin response uses the
   correctly-spelled `availableBalance` — two DIFFERENT fields in two different responses
   with two different spellings. Re-confirmed Verified-live on both doc surfaces 2026-07-14
   (ground truth "2026-07-14 Upstream Update" §5). `typos.toml` already whitelists
   `availabel`.

4. **Single-margin response = camelCase floats + STRING `leverage`.** All margin/balance
   fields are floats; `leverage` is a string like `"4.00"`, never a float.

5. **Multi-margin REQUEST naming is Dhan-side UNRESOLVED (2026-07-14 three-artifact split).**
   The classic page, the portal markdown, and the portal OpenAPI yaml disagree among
   themselves (recorded in the ground truth doc's "2026-07-14 Upstream Update"). We send the
   SDK/classic-curl-converged shape `{dhanClientId, includePosition, includeOrder, scripList}`
   — UNVERIFIED-LIVE; any first production multi-margin caller MUST live-probe the endpoint
   before trusting either naming. Never revert to `includeOrders`/`scripts` without a live
   probe proving that shape.

6. **Multi-margin RESPONSE is tolerant-parsed.** Dhan's own artifacts split between
   snake_case all-string values (classic + yaml) and camelCase floats (portal markdown).
   `MultiMarginResponse` accepts BOTH — snake_case strings AND camelCase floats — and
   normalizes every margin field to `f64`; a NON-numeric string is a hard deserialize error
   (fail-closed — garbage must never silently become 0.0). EXCEPTION: `currency` is kept as
   a RAW string (the classic live example shows `"INR"`, a currency CODE; the portal types it
   as a float margin amount — semantics Unknown until live-probed).

7. **Margin values are session-scoped — recalculate per order, never cache.** A margin
   number is indicative for the current trading session only; every entry re-runs the
   margin calculator + fund limit. No cached verdict may authorize a later order.

8. **SHARED ACCOUNT — `insufficientBalance == 0` alone is NOT authorization to spend.**
   The Dhan account is pooled with the BruteX co-tenant; `fundlimit` reflects the WHOLE
   account's balance. The margin gate therefore ADDITIONALLY applies a PER-ENTRY cap of
   `tenant_budget_percent` (hard-capped ≤ 50) of the THEN-CURRENT available balance —
   CUMULATIVE our-share is NOT capped: sequential entries each re-read the balance, so
   they can cumulatively consume more of the pool (geometrically), and the gate cannot
   distinguish OUR utilized margin from the co-tenant's (`fundlimit` is pooled), so no
   cumulative ledger exists yet (a cumulative tenant ledger is a flagged follow-up for
   the OMS-wiring PR). We never assume the full pooled margin is ours, and BruteX may
   consume margin between our check and our order (irreducible TOCTOU; the broker is
   the final arbiter).

9. **Funds/margin REST usage is self-capped at ≤ 10 req/sec** — 50% of Dhan's 20/sec
   non-trading-API budget (the same co-tenancy discipline as rule 8). The gate REFUSES an
   entry check rather than queueing when the self-cap is hit — a delayed pre-trade verdict
   is a stale verdict.

10. **The margin gate contract.**
    - **EXITS ARE NEVER MARGIN-GATED.** The exit path is structurally REST-free (no token
      read, no REST call, no limiter touch) — an exit must always be placeable.
    - Entry-check unavailability fails CLOSED for entries (degraded/implausible/self-capped
      → entry refused) and OPEN for exits (always allowed).
    - LIVE (`dry_run = false`) consumers MUST use `MarginVerdict::permits_live_entry()` — a
      DISABLED gate blocks live entries fail-closed. Paper consumers use
      `permits_paper_entry()` (a disabled gate equals today's paper behaviour).
    - Misrouting an EXIT through `check_entry` would subject it to entry gating (a
      disabled gate + `permits_live_entry` would then BLOCK a live exit — the exact hazard
      this rule forbids); the OMS-wiring PR must route intents at a single choke point
      (exits → `check_exit`) and pin it with its own ratchet.

11. **The OFF-switch lattice — config AND code lock, both required.** The gate's REST legs
    fire only when `[dhan_margin_gate] enabled` (serde default FALSE) AND the code-change
    master lock `DHAN_MARGIN_GATE_REST_ALLOWED` in `crates/common/src/constants.rs`
    (currently `false`) are BOTH true. Flipping the const requires a fresh dated operator
    quote recorded HERE first (the umbrella plan's cluster-E2 hold: the live funds/margin
    REST call awaits the operator grant). Config flips alone can never turn the REST legs
    on.

## Honest envelope / live-probe flags (2026-07-14)

- **Collateral semantics UNKNOWN:** whether `availabelBalance` includes non-cash
  collateral (`collateralAmount`) is UNKNOWN from the docs — live-probe before any live
  sizing decision trusts the balance as cash-equivalent.
- **Response-side vs request-side client-id strictness:** a fundlimit response MISSING
  `dhanClientId` (serde-default empty) is TOLERATED by the response-side check (an absent
  field is not evidence of a wrong account); the REQUEST-side check is STRICT — the gate
  refuses any entry request whose client id differs from the gate's expected id, empty
  included (the request must carry the real client id for the broker anyway).
- **Pre-open values are start-of-day values:** fundlimit values read before 09:15 IST are
  SOD balances — consumers sizing entries pre-open see start-of-day numbers, not live
  intraday utilization.
- **The REST self-cap is PER-GATE-INSTANCE:** the OMS-wiring PR must construct exactly
  ONE gate per process and pin that with its own ratchet; until then, multi-instance
  construction could exceed the documented process-wide ≤ 10 req/sec budget.

## What This Prevents

- "Fixing" the `availabelBalance` typo → fund-limit balance silently deserializes to 0.0
- Confusing the two `available*Balance` spellings → wrong field, wrong balance
- Trusting a single multi-margin request naming without a live probe → 400 on every call
- Strict single-shape multi-margin response parse → deserialization failure on the OTHER
  documented shape
- Treating pooled-account `insufficientBalance == 0` as spend authorization → co-tenant
  margin starvation
- Margin-gating an exit → a position we cannot close when the REST surface is degraded
- A caller flipping config to enable live REST calls before the operator grant → the
  constants.rs master lock blocks it at build time
- Caching a margin verdict across orders → stale session-scoped values authorizing orders

## Trigger

This rule activates when editing files matching:
- `crates/trading/src/oms/margin_gate.rs`
- `crates/trading/src/oms/types.rs`
- `crates/trading/src/oms/api_client.rs`
- Any file containing `MarginGate`, `MarginVerdict`, `OrderIntent`,
  `MarginCalculatorRequest`, `MultiMarginRequest`, `FundLimitResponse`,
  `DHAN_MARGIN_GATE_REST_ALLOWED`, `dhan_margin_gate`, `availabelBalance`,
  `margincalculator`, `fundlimit`

## 2026-07-14 — creation + coordinator directive

- Created 2026-07-14 to fix CLAUDE.md index drift: the project charter's Dhan rule index
  listed `funds-margin.md` ("`availabelBalance` typo (keep it!), string leverage") but the
  file did not exist on disk — the same drift class the 2026-07-03 docs-sync audit fixed
  for `full-market-depth.md`.
- The Funds & Margin surface was split into a dedicated session per the operator directive
  relayed via the coordinator session 2026-07-14 (the Dhan order-surface umbrella plan,
  `.claude/plans/active-plan-dhan-order-surface.md`, cluster E2).
- The margin gate ships CODE-READY and DEFAULT-OFF. Honest scope of that claim:
  - The `DHAN_MARGIN_GATE_REST_ALLOWED` const lock governs the MARGIN GATE's REST legs —
    the gate issues zero runtime REST calls until the operator grant flips the const with
    a fresh dated quote recorded in this file.
  - The raw typed wrappers on `OrderApiClient` (`calculate_margin` /
    `calculate_multi_margin` / `get_fund_limit`) are pre-existing public functions NOT
    gated by the const; they have ZERO production callers today, and the
    no-production-caller ratchet
    (`crates/trading/tests/margin_gate_off_guard.rs::test_margin_gate_and_wrappers_have_no_production_callers`)
    pins that zero until the OMS-wiring PR deliberately updates its allowlist.
  - "Zero runtime REST calls" is scoped to THIS funds/margin surface only — the spot-1m /
    option-chain per-minute legs are SEPARATELY granted Dhan REST surfaces that do run
    (`no-rest-except-live-feed-2026-06-27.md` §8).
- The `no-rest-except-live-feed-2026-06-27.md` §3 inventory rows for `fundlimit` /
  `margincalculator` are deliberately NOT added in this PR — that dated edit belongs to
  the operator's enable-grant moment (the umbrella plan's cluster-E2 hold); recorded here
  so that trigger-file drift is intentional, not an oversight.
