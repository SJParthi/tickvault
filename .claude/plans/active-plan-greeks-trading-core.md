# Implementation Plan: Consume BruteX trading-core Greeks kernel (DEC-SHARED-CORE-001 Phase 3)

**Status:** DRAFT
**Date:** 2026-07-02
**Approved by:** pending

> **Source note (ground truth):** `docs/integration/brutex-trading-core-greeks.md`
> (received 2026-07-02 from the BruteX session via the operator; kernel verified at
> bruteX rev `a09fded5b57a80b8d8659ef8f033c1ab070faa8e`).
> **This plan is DOCS-ONLY at this stage** — no Rust source changes until the operator
> approves (design-first wall + CLAUDE.md "new dep additions need Parthiban approval").

---

## Verification of the note's TickVault claims (done 2026-07-02, this worktree @ ddd1bf3d)

| Note claim | Repo reality | Verdict |
|---|---|---|
| `GreeksEnricher` trait at `crates/common/src/tick_types.rs:208-222` | Trait at `tick_types.rs:208-211` (`fn enrich(&mut self, tick: &mut ParsedTick)`); `NoopGreeksEnricher` at `:216-221` | **Verified** (range covers trait + Noop impl) |
| `run_tick_processor<G: GreeksEnricher>` at `tick_processor.rs:709` | `crates/core/src/pipeline/tick_processor.rs:709` — `pub async fn run_tick_processor<G: GreeksEnricher>(` | **Verified** |
| `build_inline_greeks_enricher` returns `Option<NoopGreeksEnricher>::None` | `crates/app/src/main.rs:9260-9266` — stub returns `None` (PR #3 retirement, 2026-05-19); call sites `main.rs:1773` + `main.rs:6186` | **Verified** |
| `rust-toolchain.toml` pins 1.95.0 | `channel = "1.95.0"` | **Verified** |
| Zero `trading-core`/`jaeckel`/`brutex` in Cargo.toml/lock | Confirmed for real deps. **Minor drift:** `jaeckel` appears in `crates/trading/Cargo.toml:51` in a cargo-machete `ignored = [...]` list — a stale leftover from the deleted greeks engine, NOT a dependency | **Verified with 1 drift note** |
| "TickVault's 9-TF live aggregator" | **Drift:** the live aggregator is **21 timeframes** (`TF_COUNT = 21`, `crates/trading/src/candles/tf_index.rs:29`; seal routing `crates/app/src/seal_routing.rs`). The STATE-not-FLOW rule applies identically — just to 20 derived TFs, not 8 | **Drift noted; no design impact** |
| Stale `OptionGreeksSnapshot` doc ref | `crates/common/src/tick_types.rs:148` — doc-comment cites deleted `greeks::black_scholes::compute_greeks()` | **Verified stale** |
| `RISK_FREE_RATE = 0.068` hardcoded | `crates/common/src/constants.rs:2034` | **Verified** |
| deny.toml needs exceptions | `deny.toml:162` `unknown-git = "deny"`, `:167` `allow-git = []` — the git source WILL be rejected today; licenses section has no `Proprietary` allowance | **Verified — Item 1 must edit both** |

### THE HONESTY CHECK — where does live IV (sigma) come from? **NO SOURCE EXISTS TODAY.**

The note's LIVE path is `sigma = Some(vendor_or_cached_iv)` — no solve. Verified repo reality:

- `ParsedTick.iv/delta/gamma/theta/vega` (`crates/common/src/tick_types.rs:52-60`) are **OUTPUT** fields, defaulting to `f64::NAN` (`:83-87`). Nothing writes them since PR #3.
- The Dhan binary feed carries **no IV field** in any packet (Ticker 16B / Quote 50B / Full 162B — see `live-market-feed.md` byte tables). The Groww live feed likewise streams price/qty ticks, not IV.
- The Dhan Option Chain REST API (the only vendor IV/greeks source TickVault ever had) was **DELETED 2026-06-28** (`option-chain.md` retirement banner; no entitlement) — and re-adding it is barred by BOTH `no-rest-except-live-feed-2026-06-27.md` (no market-data REST) and the option-chain retirement (fresh dated operator quote required).
- The current subscription universe is **spot-only** (indices + F&O underlyings + NTM constituents) — no option contracts are even subscribed, so there is no option tick to enrich (that would additionally be a `daily-universe-scope-expansion-2026-05-27.md` §12 scope question).
- The `price`-driven solve path (sigma=None, Jäckel inversion) exists in the kernel, but the note designates it HISTORICAL/BruteX-lake; using it live would need an option-premium tick + forward + expiry, none of which flow today.

**Conclusion: Item 2's enricher has NO input yet. The plan's FIRST milestone (Item 0) is the operator's IV-source decision; Items 2 and 4 are BLOCKED on it.** Items 1 and 3 (dep + golden bit-identity proof) are independently useful and unblocked once the dep is approved.

---

## Plan Items

- [ ] **Item 0 (BLOCKING DECISION): operator names the live IV (sigma) source** — no code.
  - Options (each needs its own dated operator quote per the relevant lock): (a) re-authorize a vendor IV source (e.g. Dhan Option Chain REST — currently barred by two locks), (b) subscribe option contracts + solve IV from traded premium via the kernel's sigma=None path (scope-lock §12 change + live-path solve deviates from the note's LIVE contract), (c) BruteX-side computed IV shipped to TickVault (new transport, needs design), (d) defer — land Items 1+3 only (kernel present + bit-proven, enricher OFF).
  - Files: this plan (record the decision + quote)
  - Tests: N/A — decision record
- [ ] **Item 1: workspace dependency + supply-chain exceptions** — NEEDS operator approval (CLAUDE.md: new dep additions need Parthiban approval; `^/~/*` banned — git rev pin complies as an exact pin).
  - `trading-core = { git = "https://github.com/SJParthi/bruteX", rev = "a09fded5b57a80b8d8659ef8f033c1ab070faa8e", default-features = false }` in `[workspace.dependencies]`; NEVER the `python` feature; do NOT add/override `jaeckel` (kernel pins `=0.2.0` — golden bit contract).
  - deny.toml: add the git source to `allow-git` (`:167`) + a `licenses.exceptions` entry for trading-core's `Proprietary` license.
  - Also remove the stale `jaeckel` entry from the machete `ignored` list once it becomes a real transitive dep (avoid confusion).
  - Files: Cargo.toml, deny.toml, crates/trading/Cargo.toml
  - Tests: dependency_lock_guard ratchet `crates/common/tests/trading_core_rev_pin_guard.rs` — `test_trading_core_rev_is_exact_pinned` (source-scan: full 40-char rev present, no branch/tag float); `cargo deny check` green in CI
- [ ] **Item 2: `TradingCoreGreeksEnricher` implementing the existing `GreeksEnricher` trait** — BLOCKED on Item 0.
  - LIVE path per the note: `sigma = Some(iv_from_item0_source)` — no solve on the hot path. Builds `forward = S·e^{rT}` from the cached underlying spot + `RISK_FREE_RATE`; `t_years` Act/365 from the contract expiry; maps `GreeksOut` (Copy, zero-alloc) into `tick.iv/delta/gamma/theta/vega` in place (`rho` has no ParsedTick field — Item 5 decides whether to add it). `ERR_NO_SOLUTION`/`ERR_INVALID` ⇒ leave fields NAN + static-label counter (`tv_greeks_enrich_skipped_total{reason}`) — honest NA, never a fabricated value; `invalid_input` additionally logs `error!` with a new typed ErrorCode + rule-file mention (cross-ref test).
  - Config flag `[greeks] enabled = false` DEFAULT OFF (common-runtime rule; byte-identical behaviour until flipped) — `build_inline_greeks_enricher` returns `Some(TradingCoreGreeksEnricher)` only when enabled AND an IV source is wired; otherwise `None` exactly as today.
  - Files: crates/trading/src/greeks_enricher.rs (new), crates/common/src/config.rs, config/base.toml, crates/app/src/main.rs (build_inline_greeks_enricher), crates/common/src/error_code.rs, .claude/rules/project/greeks-trading-core-error-codes.md (new)
  - Tests: test_enricher_maps_greeks_out_into_parsed_tick, test_enricher_leaves_nan_on_no_solution, test_flag_off_returns_none, DHAT `crates/trading/tests/dhat_greeks_enrich.rs` (zero-alloc per enrich), Criterion bench `greeks_enrich` + `quality/benchmark-budgets.toml` entry
- [ ] **Item 3: golden bit-identity test ported verbatim** (unblocked by Item 1 alone).
  - Copy bruteX `tests/golden/fixtures/greeks_golden.json` into `crates/trading/tests/golden/fixtures/`; port `rust/trading-core/tests/greeks_golden.rs` verbatim (only the fixture path changes). 24 cases × {solve, sigma} paths; every field ≤ 4 ULP of the frozen bits AND within the mpmath dps=50 tolerance (rel 1e-11 / abs 1e-12). Proves bit-identity on TickVault's 1.95.0 toolchain + link product.
  - Files: crates/trading/tests/greeks_golden.rs (new), crates/trading/tests/golden/fixtures/greeks_golden.json (new)
  - Tests: greeks_golden (24-case suite) — the test IS the deliverable
- [ ] **Item 4: derived-TF STATE-not-FLOW greeks stamping** — BLOCKED on Items 0+2.
  - At the 1m seal, stamp greeks once (sigma=Some path); every derived TF (the other 20 of `TF_COUNT = 21`) takes the LAST constituent 1m bar's values by last-value snapshot — never summed/averaged/OHLC'd; NULL propagates (last 1m NULL ⇒ derived NULL, no backfill from earlier minutes). O(1) per bar.
  - **§28 indicator-boundary statement (explicit):** this touches the AGGREGATOR/candle chain (`crates/trading/src/candles/`, `crates/app/src/seal_routing.rs`, seal-writer wiring) — NOT the frozen `crates/trading/src/indicator/` or `crates/trading/src/strategy/` files. `daily-universe-scope-expansion-2026-05-27.md` §28 froze indicators+strategies only (its own Sub-PR #8 was re-scoped to "aggregator-only"), so this item does not require a §28 lift — but the `operator_boundary_indicator_strategy_guard.rs` manifest is re-checked in review to prove zero frozen-file diffs.
  - Files: crates/trading/src/candles/ (seal path), crates/app/src/seal_routing.rs, crates/storage (candle schema `ALTER ADD COLUMN IF NOT EXISTS` for greeks columns + DEDUP keys unchanged)
  - Tests: test_derived_tf_greeks_are_last_1m_snapshot, test_null_greeks_propagate_to_derived_tf, test_greeks_never_averaged_ratchet (source-scan)
- [ ] **Item 5: note follow-ups (small, unblocked after Item 1)**
  - Fix stale doc-comment `crates/common/src/tick_types.rs:148` (references deleted `greeks::black_scholes::compute_greeks()`) → point at trading-core.
  - `RISK_FREE_RATE = 0.068` (`crates/common/src/constants.rs:2034`) seam alignment with BruteX's `RiskFreeSource`: document the constant as the Phase-3 `rate` input; propose a config-sourced rate (`[greeks] risk_free_rate`) so both systems can align — decide with operator whether a `rho` field is added to `ParsedTick`/`OptionGreeksSnapshot` (the note flags the old snapshot has no rho).
  - Files: crates/common/src/tick_types.rs, crates/common/src/constants.rs, config/base.toml
  - Tests: existing constants boundary test extended (`test_risk_free_rate_within_band`), doc-link check

## Open questions for the operator (answers gate everything)

1. **Approve the git dependency on bruteX?** `trading-core` via `git = "https://github.com/SJParthi/bruteX"` at exact rev `a09fded5b57a80b8d8659ef8f033c1ab070faa8e`, `default-features = false`, plus deny.toml exceptions (git source + Proprietary license). CLAUDE.md requires your explicit approval for any new dep. (A private-repo git dep also means CI needs read access to bruteX — confirm the GitHub token/deploy-key path.)
2. **Where does live IV (sigma) come from?** Honest answer: **nowhere, today.** Dhan/Groww live feeds carry no IV; the option-chain REST (the only vendor greeks source) was deleted 2026-06-28 and is barred by the no-REST lock; no option contracts are subscribed. Pick Item 0 option (a) re-authorize a vendor IV source, (b) subscribe options + solve from premium (scope + live-solve deviation), (c) BruteX-computed IV transport, or (d) defer — land dep + golden proof only, enricher stays OFF.

---

## Design

The integration is the smallest correct consumption of the shared kernel, phased so each PR is independently green:

1. **Dependency layer (Item 1):** trading-core enters `[workspace.dependencies]` as an exact-rev git pin with `default-features = false` (no pyo3, no libpython — proven by the BruteX-side `ldd` check). `jaeckel =0.2.0` arrives transitively and is never overridden (frozen golden bit contract). Supply-chain gates (`cargo deny`) are taught the two facts they will otherwise reject: the git source and the Proprietary license.
2. **Proof layer (Item 3):** the 24-case golden bit-identity test runs on TickVault's own toolchain/link product BEFORE any production code consumes the kernel — numeric drift is caught by CI, not by a trading day.
3. **Enricher layer (Item 2):** a `TradingCoreGreeksEnricher` slots into the EXISTING seam — the `GreeksEnricher` trait (`tick_types.rs:208`) consumed monomorphically by `run_tick_processor<G>` (`tick_processor.rs:709`). No pipeline redesign; `build_inline_greeks_enricher` (`main.rs:9260`) becomes the single construction point, gated by a default-OFF config flag. LIVE path is `sigma = Some(..)` (no solve, pure closed-form evaluation — O(1), `GreeksOut` is Copy/zero-alloc). Hot-path budget enforced by DHAT + Criterion per `hot-path.md`.
4. **Derived-TF layer (Item 4):** greeks are STATE, not FLOW — stamped at the 1m seal, last-value-snapshotted to the 20 derived TFs, matching bruteX §15.10 so live == backtest bit-for-bit.
5. **Sequencing:** Item 0 (IV-source decision) → Item 1 (dep) → Item 3 (proof) → Item 2 (enricher) → Item 4 (derived TF) → Item 5 (cleanups). Items 2 and 4 cannot start until Item 0 is answered; Items 1/3/5 can land first as a "kernel present + proven, OFF" milestone.

## Edge Cases

- **No IV available for a tick** (source stale/missing): enricher leaves NAN (the existing ParsedTick default), increments a reason-labelled counter — never blocks the tick, never fabricates.
- **`t_years < T_FLOOR_YEARS` (expiry within 600s):** kernel returns `no_solution` — honest NA, mapped to NAN; expected every expiry afternoon.
- **Deep-ITM theta positive when r > 0:** correct kernel behaviour per the note — no clamping in TickVault.
- **`admissible = false`** (|delta| < 0.02 or raw vega < 1e-6): a FILTER flag, not an error — greeks are still stamped; downstream consumers decide.
- **sigma outside [0.001, 5.0] from the IV source:** kernel rejects as `invalid_input` — caller-bug class, logged loudly with typed ErrorCode.
- **Non-option ticks (indices/equities):** enricher no-ops exactly like today's contract ("index/equity ticks: updates internal underlying LTP cache, no Greeks").
- **Derived-TF NULL propagation:** last constituent 1m bar NULL ⇒ derived bar NULL; never backfilled from an earlier minute (stale-spot fabrication banned).
- **IST-midnight / 15:30 force-seal burst:** greeks stamping is O(1) per seal and rides the existing `SEAL_BUFFER_CAPACITY` envelope — no new burst amplification.
- **Toolchain/link-product numeric drift:** the ≤4 ULP golden envelope catches it; the mpmath oracle catches a "both drifted together" false pass.

## Failure Modes

- **bruteX repo unreachable at build time (git dep):** build fails loudly at `cargo fetch` — Cargo.lock pins the rev, and CI caches the registry, but a cold CI run needs bruteX read access. Mitigation: confirm token/deploy-key (open question 1); fallback: vendor the fixture + wait — never float to a mirror silently.
- **Rev bumped accidentally / branch float:** rejected by the `trading_core_rev_pin_guard.rs` source-scan ratchet + `cargo deny` unknown-git.
- **Kernel returns Err on the hot path:** enricher maps to NAN + counter + (for `invalid_input`) `error!` with typed code — tick flow is NEVER blocked; zero-tick-loss chain untouched.
- **Golden test fails on our toolchain:** hard CI failure BEFORE the enricher ships (Item 3 lands first) — the kernel is not consumed in production until bit-identity is proven.
- **pyo3/libpython accidentally linked:** prevented by `default-features = false` + a ratchet asserting the `python` feature never appears in Cargo.toml/lock.
- **IV source dies mid-session (once one exists):** greeks degrade to NAN with a rising-edge alert (edge-triggered per audit Rule 4); ticks/candles unaffected.
- **§28 frozen-file drift:** `operator_boundary_indicator_strategy_guard.rs` fails the build if Item 4 accidentally touches indicator/strategy files.

## Test Plan

- **Item 1:** `cargo deny check` green; `trading_core_rev_pin_guard.rs::test_trading_core_rev_is_exact_pinned`; `cargo build --workspace` proves no pyo3 in the tree (`cargo tree -i pyo3` empty ratchet).
- **Item 3:** the ported 24-case golden suite (`greeks_golden.rs`) — every field ≤ 4 ULP of frozen bits + mpmath tolerance; runs in the normal `cargo test -p tickvault-trading` scope + CI.
- **Item 2:** unit tests (mapping, NAN-on-error, flag-off ⇒ None), DHAT zero-alloc test (`dhat_greeks_enrich.rs`), Criterion bench with a `quality/benchmark-budgets.toml` budget + 5% regression gate; error-code cross-ref + tag-guard tests for the new variant; pub-fn-test + pub-fn-wiring gates.
- **Item 4:** seal-path unit tests (last-value snapshot, NULL propagation), source-scan ratchet banning sum/avg of greeks fields, schema DDL test (`ALTER ADD COLUMN IF NOT EXISTS` per B10), DEDUP keys unchanged (dedup_segment_meta_guard stays green).
- **Item 5:** constants boundary test; doc builds clean.
- Scope: changes touch `crates/common` (config/error_code) ⇒ workspace escalation per `testing-scope.md` on those PRs; otherwise crate-scoped.

## Rollback

- **Item 1:** revert the Cargo.toml + deny.toml + Cargo.lock hunk — single-commit revert, no runtime surface.
- **Item 2:** config flag `[greeks] enabled = false` is the FIRST rollback (no deploy needed — default is already OFF); full revert removes the enricher module and `build_inline_greeks_enricher` returns `None` again (today's exact code).
- **Item 3:** deleting the test file removes the proof but breaks nothing.
- **Item 4:** greeks columns are additive (`ALTER ADD COLUMN IF NOT EXISTS`); rollback stops stamping (columns read NULL) — no DROP, no DEDUP-key change, no data loss.
- Kernel rev bump rollback: restore the previous 40-char rev; the golden test certifies either direction.

## Observability

- Counters: `tv_greeks_enrich_total{outcome="ok"}`, `tv_greeks_enrich_skipped_total{reason="no_solution"|"invalid_input"|"no_iv"|"disabled"}` — static labels, zero-alloc emission.
- Typed ErrorCode (new, e.g. `GREEKS-CORE-01` for `invalid_input`-class caller bugs) + rule-file runbook `.claude/rules/project/greeks-trading-core-error-codes.md` — satisfies `error_code_rule_file_crossref.rs` + `error_code_tag_guard.rs` in the same PR.
- Tracing: `#[instrument]`-free hot path (per hot-path rules) — a coalesced per-minute summary log instead of per-tick spans.
- Bench + DHAT artifacts in CI (post-merge gates) prove the O(1)/zero-alloc claim continuously.
- Telegram: rising-edge alert only for a sustained IV-source outage (once a source exists); no per-tick noise (10 commandments).
- Audit: greeks values land in the candle rows themselves (STATE) — queryable via `questdb_sql`; no separate audit table needed for pure derived values (cross-referenced in review; if the operator wants a provenance column, it is an additive SYMBOL column).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Flag OFF (default) | Byte-identical to today: enricher = None, all greeks NAN |
| 2 | Flag ON, no IV source wired | Boot refuses to construct the enricher (falls back to None) + one loud boot log — no false-OK |
| 3 | Flag ON, IV present, valid option tick | tick.iv/delta/gamma/theta/vega stamped from GreeksOut; O(1), zero-alloc |
| 4 | Kernel `no_solution` (expiry < 600s / no-arb bound) | NAN + counter, tick flows normally |
| 5 | Kernel `invalid_input` | NAN + counter + `error!` typed code (caller bug — loud) |
| 6 | 1m seal with greeks → 5m/15m/…/1d derived bars | Derived bars carry EXACTLY the last 1m bar's greeks (STATE-not-FLOW) |
| 7 | Last 1m bar greeks NULL | All derived bars NULL for that window — no backfill |
| 8 | Toolchain upgrade (e.g. 1.96) | Golden 24-case suite re-certifies ≤4 ULP or fails CI |

## Per-Item Guarantee Matrix

The 15-row "100% everything" matrix and the 7-row Resilience Demand Matrix from
`.claude/rules/project/per-wave-guarantee-matrix.md` apply to EVERY item above by
cross-reference, with these item-specific bindings:

### 15-row 100% Guarantee Matrix (binding per `per-wave-guarantee-matrix.md`)

| Demand | Mechanical proof artefact | Real-time check | Per-item gate |
|---|---|---|---|
| 100% code coverage | `quality/crate-coverage-thresholds.toml` 100% min; `scripts/coverage-gate.sh` | post-merge llvm-cov | Items 2/3/4 add full-coverage tests |
| 100% audit coverage | greeks are STATE columns on candle rows (DEDUP UPSERT KEYS unchanged); provenance column optional | `mcp__tickvault-logs__questdb_sql` | Item 4 schema via ALTER ADD COLUMN IF NOT EXISTS |
| 100% testing coverage | unit + property (golden 24-case) + DHAT + Criterion + source-scan ratchets, per `testing.md`, crate-scoped | `cargo test --workspace` green | each item names its tests above |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan + pre-commit gates | pre-push mandatory | all gates green per PR |
| 100% code performance | DHAT zero-alloc (`dhat_greeks_enrich.rs`) + Criterion budget entry + bench-gate ≤5% | `cargo bench` + `scripts/bench-gate.sh` | Item 2 is hot path ⇒ DHAT+bench mandatory |
| 100% monitoring | counters `tv_greeks_enrich_*` + tracing + candle-row audit (7-layer, CloudWatch-only era) | `mcp__tickvault-logs__run_doctor` | Item 2 wires layers |
| 100% logging | `error!` with `code = ErrorCode::GreeksCore01...code_str()` | hourly errors.jsonl | tag-guard enforced |
| 100% alerting | rising-edge IV-outage alert (CloudWatch) | `run_doctor` alarms | Item 2 (once source exists) |
| 100% security | no secrets touched; git-dep supply chain gated by cargo deny + rev-pin ratchet; security-reviewer pass | `cargo audit` | Item 1 |
| 100% security hardening | `default-features = false`, no pyo3/libpython ratchet, no network at runtime from the kernel | post-merge deny | Item 1 |
| 100% bugs fixing | adversarial 3-agent review before AND after impl (Heavy tier — multi-crate) | pre-PR + post-impl | every code item |
| 100% scenarios covering | the 8-scenario table above; chaos untouched (no new drop path) | chaos suite stays green | Items 2/4 |
| 100% functionalities covering | every new pub fn has call site + test | pre-push gates 6+11 | every code item |
| 100% code review | 3-agent on diff before AND after | per-PR | every code item |
| 100% extreme check | rev-pin ratchet, no-pyo3 ratchet, never-averaged ratchet, golden suite — all fail the build on regression | every commit | Items 1/3/4 |

### 7-row Resilience Demand Matrix

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | Enricher NEVER blocks or drops a tick — errors map to NAN in place; ring→spill→DLQ untouched | Item 2 introduces no tick-drop path (review + chaos suite green) |
| WS never disconnects | No WS surface touched; 2-WS lock + SubscribeRxGuard unchanged | all items — zero WS diffs |
| Never slow/locked/hanged | sigma=Some closed-form eval, GreeksOut Copy, zero-alloc; DHAT + Criterion budget | Item 2 DHAT/bench |
| QuestDB never fails | Greeks columns additive; schema self-heal (`ALTER ADD COLUMN IF NOT EXISTS`) preserved | Item 4 |
| O(1) latency | O(1) per tick (enrich) and per bar (last-value snapshot); the golden suite is test-time O(24), not runtime | Items 2/4 bench; honest O-flags |
| Uniqueness + dedup | No DEDUP key changes; composite `(security_id, exchange_segment)` untouched; dedup_segment_meta_guard green | Item 4 |
| Real-time proof | counters + run_doctor + candle rows queryable | Item 2 |

## Honest envelope (mandatory wording per operator-charter §F)

100% inside the tested envelope, with ratcheted regression coverage: the kernel's
numeric contract is certified on TickVault's exact toolchain by the ported 24-case
golden bit-identity suite (≤4 ULP vs frozen f64 bits + mpmath dps=50 oracle at rel
1e-11 / abs 1e-12 — ratchet `greeks_golden.rs`); the dependency is exact-rev-pinned
(`a09fded5b57a80b8d8659ef8f033c1ab070faa8e`, ratchet `trading_core_rev_pin_guard.rs`)
with `default-features = false` and a no-pyo3 tree ratchet; the enricher is default-OFF,
zero-alloc (DHAT-ratcheted), O(1) per tick, and maps every kernel error to honest NAN —
it can never block, drop, or reorder a tick, so the existing ring→spill→DLQ zero-tick-loss
chain is untouched. Beyond the envelope: we make NO claim about IV correctness until an
IV source exists (open question 2 — today there is none), and NO claim that greeks are
produced at all while the flag is OFF or the source is absent — those states are honest
NAN + counter, never a fabricated value.
