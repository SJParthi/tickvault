> **Received 2026-07-02 from the BruteX session via the operator (DEC-SHARED-CORE-001 Phase 3 handoff).**
> **Status: Phase-3 plan is DRAFT — pending operator approval** (see `.claude/plans/active-plan-greeks-trading-core.md`).
> **The note below is copied VERBATIM from the BruteX session; TickVault-side verification notes live in the plan, not here.**

# TickVault × BruteX shared Greeks kernel — integration note (verified 2026-07-02)

## Context

The shared `rust/trading-core/` Greeks kernel was verified natively consumable at bruteX `main` = `a09fded5` (full rev `a09fded5b57a80b8d8659ef8f033c1ab070faa8e`), 2026-07-02. PyO3 is fully feature-gated (`default = []`, `python = ["dep:pyo3"]` opt-in only, activated solely by maturin for the BruteX wheel); the crate's sole mandatory runtime dependency is `jaeckel = "=0.2.0"` (pure Rust, zero transitive deps, exact-pinned because the solver float path is part of the frozen golden bit contract). An external standalone consumer crate depending on trading-core with `default-features = false` was built as proof: it compiled, called `greeks`/`black76_price`, all assertions passed, and `ldd` confirmed **zero libpython linked**. With `cargo test --no-default-features --locked` at that rev, the full native suite is green: the 24-case golden bit-identity test (24 cases × solve/sigma paths, ≤ 4 ULP vs the frozen f64 bit patterns plus the mpmath dps=50 independent oracle at rel 1e-11 / abs 1e-12), 10 property tests, and 28 greeks unit tests.

## Finding: TickVault does not consume trading-core yet

TickVault @ `15d7ee7` does **NOT** consume trading-core today — the DEC-SHARED-CORE-001 Phase-3 native link has never landed (zero hits for `trading-core`/`trading_core`/`jaeckel`/`brutex` in its workspace `Cargo.toml`, any crate manifest, or `Cargo.lock`). TickVault's own former greeks engine was **deleted in its PR #3** (2026-05-19: the `greeks_pipeline` module removed, `InlineGreeksComputer` retired, and `build_inline_greeks_enricher` now returns `Option<NoopGreeksEnricher>::None`). What remains is the exact injection seam trading-core should plug into: the **`GreeksEnricher` trait** (`fn enrich(&mut self, tick: &mut ParsedTick)`) at `crates/common/src/tick_types.rs:208-222`, consumed monomorphically by `run_tick_processor<G: GreeksEnricher>` (`crates/core/src/pipeline/tick_processor.rs:709`). Toolchain compatibility is satisfied: TickVault's `rust-toolchain.toml` pins Rust **1.95.0**, which meets trading-core's `rust-version = "1.93.1"` floor; both are edition 2024.

## Integration note (copy-paste into TickVault)

## Consuming the shared BruteX Greeks kernel (trading-core) — DEC-SHARED-CORE-001 Phase 3

### Cargo.toml (workspace root)
[workspace.dependencies]
# Pin the exact rev (Golden Rule #4: reproducibility). Bump deliberately, never float.
trading-core = { git = "https://github.com/SJParthi/bruteX", rev = "a09fded5b57a80b8d8659ef8f033c1ab070faa8e", default-features = false }
# default = [] already, but default-features = false is belt-and-suspenders:
#   - NEVER enable the `python` feature (pulls pyo3 + links CPython symbols).
#   - Enable `features = ["serde"]` ONLY if you (de)serialize StrategySpec.
# Do NOT add/override `jaeckel` — trading-core pins `=0.2.0`; the solver float
# path is part of the frozen golden bit contract. A version bump moves bits.
# supply-chain: add deny.toml exceptions for `license = "Proprietary"` + the git source.
# MSRV: rust-version 1.93.1 (your 1.95.0 pin satisfies it); edition 2024.

### Public API (all native, no Python)
use trading_core::{greeks, black76_price, GreeksOut};

pub fn greeks(
    price: Option<f64>,   // traded premium (discounted, rupees). Required iff sigma = None
    sigma: Option<f64>,   // known annualized IV in [0.001, 5.0]. Some(σ) ⇒ price IGNORED
    forward: f64,         // F = S·e^((r−q)T) — YOU build it; kernel takes forward, not spot
    strike: f64,
    t_years: f64,         // Act/365 (SECONDS_PER_YEAR = 365·86400); must be ≥ T_FLOOR_YEARS
    rate: f64,            // continuously-compounded annual r, |r| ≤ 1.0
    is_call: bool,        // true = CE, false = PE
) -> Result<GreeksOut, String>;

pub fn black76_price(sigma, forward, strike, t_years, rate, is_call) -> f64; // non-validating pricer

pub struct GreeksOut {  // Copy, zero-alloc
    pub iv: f64,        // annualized decimal (0.18 = 18%)
    pub delta: f64,     // signed: CE ∈ [0,1], PE ∈ [−1,0]
    pub gamma: f64,     // ≥ 0, per unit of F, side-independent
    pub theta: f64,     // per CALENDAR day, Act/365 (deep-ITM theta CAN be positive when r > 0)
    pub vega: f64,      // per 1 vol-POINT (per 1% IV) = raw/100, ≥ 0
    pub rho: f64,       // per 1% rate, BSM-form (CE +, PE −)  ← your old snapshot had no rho
    pub price: f64,     // Black-76 discounted price at `iv`
    pub admissible: bool, // |delta| ≥ 0.02 AND raw vega ≥ 1e-6 — a FILTER, not an error
}

### Two paths, one kernel
- LIVE (TickVault hot path / GreeksEnricher::enrich): sigma = Some(vendor_or_cached_iv) — no solve.
- HISTORICAL (BruteX lake): sigma = None — Jäckel LBR inversion, round-trip verified to 1e-9 rel,
  deterministic bracketed Newton-bisection fallback (≤ 80 iters). Same closed form prices both.

### Error taxonomy (machine-readable prefixes; kernel NEVER panics)
- "invalid_input: ..."  (ERR_INVALID)     → caller bug: non-finite/non-positive/subnormal F, K, price;
                                             |rate| > 1; sigma outside [0.001, 5.0]; both price & sigma None.
                                             RAISE/log loudly.
- "no_solution: ..."    (ERR_NO_SOLUTION) → honest NA: t_years < T_FLOOR_YEARS (600 s / 31 536 000);
                                             price below intrinsic or ≥ upper no-arb bound (F call / K put);
                                             solve outside the [IV_CLAMP_MIN=0.001, IV_CLAMP_MAX=5.0] clamp
                                             (the boundary is NEVER reported as "the IV"); LBR non-convergence.
                                             Map to NaN/None (BruteX stamps NULL + provenance).

### Golden fixture contract (prove bit-identity on YOUR toolchain)
- Oracle: bruteX repo `tests/golden/fixtures/greeks_golden.json` — 24 cases × {solve, sigma} paths,
  frozen f64 bit patterns (Linux x86_64 wheel) + independent mpmath dps=50 reference (rel 1e-11, abs 1e-12).
- Port bruteX `rust/trading-core/tests/greeks_golden.rs` verbatim (only the fixture path changes):
  assert every field ≤ 4 ULP of the frozen bits (measured cross-link-product envelope is 2 ULP; the
  sigma path is bit-identical) AND within the mpmath tolerance. Any real numeric regression fails both.

### STATE-not-FLOW derived-TF rule (live == backtest bit-for-bit)
Locked semantics: bruteX docs/GREEKS_ENGine_PLAN.md §15.10, DEC-GREEKS-ENGINE-PR1-001 (MIGRATION.md #1094).
- Greeks/IV are STATE, not FLOW — same rule as `close`. A derived N-minute bar's
  iv/delta/gamma/theta/vega/rho (+ provenance) are EXACTLY the stored values of its LAST
  constituent 1-minute bar. NEVER summed, NEVER averaged, NEVER OHLC-aggregated.
- NA propagates honestly: last constituent bar NULL ⇒ derived bar NULL. Never backfill from an
  earlier minute (stale-spot fabrication). Zero-constituent bars don't exist (resampler emits none).
- No IV OHLC series is defined — close-snapshot only.
For TickVault's 9-TF live aggregator: stamp Greeks once per 1-minute seal via the sigma=Some path,
then propagate to every derived TF by last-value snapshot. That is mathematically identical to
recomputing at the derived bar's close instant, O(1) per bar, and matches BruteX's backtest lake.

## Follow-up suggestions (not done, out of scope)

1. TickVault's `OptionGreeksSnapshot` doc still references the deleted `greeks::black_scholes::compute_greeks()` — stale.
2. Its hardcoded `RISK_FREE_RATE=0.068` has no equivalent of BruteX's `RiskFreeSource` seam — worth aligning when Phase 3 lands.
3. BruteX could ship the golden fixture inside `rust/trading-core/` (or via `include_str!`) to remove the repo-relative test path.
