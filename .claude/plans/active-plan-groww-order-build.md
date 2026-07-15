# Implementation Plan: Groww Order-Side Build — Gated Live-Fire Lattice (PR-0 scaffold + area PRs)

**Status:** VERIFIED
**Date:** 2026-07-14
**Approved by:** Parthiban (operator) — DIRECT dated in-thread confirmation 2026-07-14 (session event `321350d4-cf70-4a1c-88fc-36584527c8fd`), verbatim: "confirm — apply the Groww order scope-unlock PR-0. Build only, behind the OFF switch, no live orders"

> **Operative authorization — the operator's DIRECT dated in-thread confirmation (2026-07-14, session event `321350d4-cf70-4a1c-88fc-36584527c8fd`), verbatim:**
> "confirm — apply the Groww order scope-unlock PR-0. Build only, behind the OFF switch, no live orders"
>
> **Originating build directive (same day, relayed verbatim via the coordinator session):**
> "build design architect and even integrate everything entirely… only when I turn on/off or enable live orders then live orders should be placed."

> **Target file:** `.claude/plans/active-plan-groww-order-build.md`
> **Authority:** CLAUDE.md > `operator-charter-forever.md` > `groww-second-feed-scope-2026-06-19.md` §39 > `no-rest-except-live-feed-2026-06-27.md` §10 > this plan.
> **Scope note (design-first wall):** touched crates — `crates/common` (`broker_order_events.rs`, `config.rs`, `constants.rs`, lattice guard), `crates/trading` (`oms/mod.rs`, `oms/groww/*`), plus rule/doc files. dry_run stays `true`; the §28 strategy/indicator boundary stays FROZEN; the Groww shared token-minter lock (2026-07-02) is UNCHANGED.

---

## Design

The Groww order-side (orders, smart orders, portfolio, margin, user profile, exceptions) is built and integrated **behind a 4-gate live-fire lattice** so that ZERO Groww order-side request can fire until the operator's explicit, future, dated live-orders enable. The four gates (per §39.2), each independently build-failure-ratcheted by `crates/common/tests/groww_order_lattice_guard.rs`:

1. **Gate 1 — config default-OFF.** A `[groww_orders]` config section (`GrowwOrdersConfig`) whose every key defaults `false`. Read-only order/portfolio/margin/user GETs run ONLY when their per-area flag is `true` AND during market hours (the cold-path scheduled-read discipline). `live_fire_requested` is a declared-intent flag that is INERT without Gate 3.
2. **Gate 2 — non-default cargo feature `groww_orders`.** The entire `crates/trading/src/oms/groww/` subtree compiles ONLY under this feature, absent from every default/prod/CI build.
3. **Gate 3 — hardcoded const `GROWW_ORDER_LIVE_FIRE = false`.** Every mutating order path (`/v1/order/create` · `/modify` · `/cancel` · `/v1/order-advance/*`) fail-closed refuses to send while this source const is `false`, independent of any config value.
4. **Gate 4 — rule lock.** §39 (`groww-second-feed-scope`) + §10 (`no-rest-except-live-feed`) hold the contract; flipping live fire requires a fresh dated operator quote editing §39 + Gate 3 together FIRST.

**PR-0 (this scaffold)** lands: the consolidated §39 / §10 scope grants; the neutral `BrokerOrderEvent` / `BrokerOrderStatus` seam (reusing the existing `Feed` enum for the broker id — no duplicate enum); the `GrowwOrdersConfig` (Gate 1) + `[groww_orders]` base.toml block; the `GROWW_ORDER_LIVE_FIRE` const (Gate 3); the `oms/groww/mod.rs` stub + `groww_orders` feature on both Cargo.tomls (Gate 2); and the lattice guard. **Area PRs** follow serially on the non-overlapping files of the §39.3 collision contract (Orders → `oms/groww/api_client.rs` / `GROWW-ORD-*`; Smart Orders → `smart_orders.rs` / `GROWW-OCO-*`; Portfolio → `portfolio.rs` / `GROWW-PORT-*`; Margin → `margin.rs` / `GROWW-MARG-*`; User+Exceptions → `user.rs` / `GROWW-READY-*`). Shared files (`error_code.rs`, `config.rs`, `base.toml`, `events.rs`, `oms/mod.rs`, `oms/groww/mod.rs`) are touched by the build lead only, serially.

Broker doc reference: `docs/groww-ref/16-orders-margins-portfolio.md` (the build reference per §39; endpoints, the 22-field detail schema, the 12-value + `OPEN` status annexure §4.1, the GA error codes §5).

## Edge Cases

- **Un-mapped broker status string** → `BrokerOrderStatus::Unknown` (never a panic), with `raw_status` preserving the original wire value — the undocumented Groww `OPEN` and any future Groww/Dhan status are absorbed, never lost.
- **Absent `[groww_orders]` config section / a TOML written before this build** → fully disabled (every gate reads dark).
- **`live_fire_requested = true` in config with Gate 3 still `false`** → fires NOTHING (config alone can never place a live order).
- **A default (feature-off) build** → the `oms/groww` subtree does not exist; the `broker_order_event` seam compiles but issues no request.
- **Read-only GET flag on outside market hours** → refused (market-hours gate); no off-hours order-side polling.
- **Case / whitespace variance in status strings** → normalized (trim + ASCII-uppercase) before mapping.
- **Groww order-side path string leaking into a non-`oms/groww` source file** → Gate-5 workspace scan fails the build.

## Failure Modes

- **Ungated order call site introduced anywhere** → `groww_order_lattice_guard.rs` Gate 5 build failure.
- **A gate weakened** (config key flipped `true` in base.toml / feature added to a default set / const flipped / §39-§10 marker removed) → the corresponding Gate 1–4 ratchet fails the build.
- **Groww REST failure at runtime** (a future area PR's concern): fail-soft per-area error codes (`GROWW-ORD-*` … `GROWW-MARG-*`), bounded retry with backoff, never a retry storm; a mutating request is refused (not retried) while the lattice is closed.
- **Token stale / auth reject** → the read-only paths degrade loud; the shared minter (2026-07-02 lock) is never minted from here.
- **Silent success on empty compare** → forbidden (audit Rule 11): a readiness surface reports partial/blind honestly.

## Test Plan

- `broker_order_events.rs` inline `#[cfg(test)]`: every documented Groww status (§4.1 twelve values + `OPEN`) and every Dhan `OrderStatus` value map non-`Unknown`; garbage/empty → `Unknown` (no panic); case + whitespace handling; `as_str` / `is_terminal` totality; `BrokerOrderEvent` raw-status preservation + integer-paise fill. Every pub fn has a test (pub-fn-test-guard).
- `groww_order_lattice_guard.rs`: Gate 1 (`GrowwOrdersConfig::default()` + base.toml keys all false), Gate 2 (feature present in both manifests, never default), Gate 3 (const false + source literal), Gate 4 (§39 / §10 markers + guard cross-ref), Gate 5 (no ungated call site in `crates/*/src`).
- `config` default-off unit test (mirrors `test_strategy_dry_run_defaults_true`).
- Scoped `cargo test -p tickvault-common` + `-p tickvault-trading` (testing-scope.md); `crates/common` change escalates to workspace per testing-scope rule.
- Area PRs add their own 22-test-category coverage per changed crate.

## Rollback

- **Config rollback (no code change):** every `[groww_orders]` key stays `false` → order-side dark; a default build already excludes the `oms/groww` subtree (feature off).
- **Feature rollback:** the `groww_orders` cargo feature is never in a default set, so no rollback is needed to keep prod dark; removing the feature line is caught by Gate 2 (it must exist in both manifests) — the intended rollback is simply "do not enable the feature".
- **Live-fire rollback:** `GROWW_ORDER_LIVE_FIRE` stays `false`; flipping back to `false` (if ever set) is a one-line source revert + a dated §39 note.
- Every gate is independently reversible; none touches the live tick/candle path, `dry_run`, or the §28 boundary.

## Observability

- Per-area error-code prefixes (`GROWW-ORD-*` / `GROWW-OCO-*` / `GROWW-PORT-*` / `GROWW-MARG-*` / `GROWW-READY-*`) land WITH each area PR (the `error_code_rule_file_crossref.rs` + `error_code_tag_guard.rs` discipline); readiness reuses the existing SPOT1M/CHAIN codes per its design.
- The neutral `BrokerOrderEvent` carries `raw_status` + `received_at_ms` for forensic audit and freshness.
- Order-audit table + reconciliation (future area PRs) map through the neutral seam so audit/metrics speak one order shape across brokers.
- PR-0 itself adds no runtime emit site (scaffold only); its observability is the four build-failing gate ratchets.

---

## Guarantee Matrices (per `.claude/rules/project/per-wave-guarantee-matrix.md`)

This plan and every item in it carry the canonical **15-row 100% Guarantee Matrix** and the **7-row Resilience Demand Matrix** from `per-wave-guarantee-matrix.md` — reproduced by cross-reference below; all 15 + 7 rows apply to every item.

**15-row 100% matrix (sentinels present for the mechanical check):** 100% code coverage, 100% audit coverage, 100% testing coverage, 100% code checks, 100% code performance, 100% monitoring, 100% logging, 100% alerting, 100% security, 100% security hardening, 100% bugs fixing, 100% scenarios covering, 100% functionalities covering, 100% code review, 100% extreme check — each proven per the artefacts named in `per-wave-guarantee-matrix.md`.

**7-row resilience matrix:** Zero ticks lost (this scaffold introduces NO tick-drop path — cold/scaffold only), WS never disconnects (untouched), never slow/locked/hanged (no hot-path allocation added), QuestDB never fails (untouched), O(1) latency (the neutral status mappers are O(1) total matches), uniqueness + dedup (broker id via the `Feed` enum; feed-in-key discipline preserved for any future order-audit table), real-time proof (the four build-failing gate ratchets).

**Honest 100% claim (mandatory wording):** any "100% guarantee" in this work means **100% inside the tested envelope, with ratcheted regression coverage** — the four gates are build-failure-ratcheted; live order fire is impossible until a dated operator enable aligns all four. Zero Groww order request fires from this scaffold; the seam is pure data + total no-panic mappers. NOT claimed: live Groww order behaviour (UNVERIFIED-LIVE), or that our Groww API key carries order entitlement (UNPROBED) — the first authorized live session is the probe.

---

## Plan Items

- [x] Item 0 — Consolidated scope grants (rule locks, Gate 4)
  - Files: `.claude/rules/project/groww-second-feed-scope-2026-06-19.md` (append §39 + §1/§4/§38.4 annotations), `.claude/rules/project/no-rest-except-live-feed-2026-06-27.md` (append §10 + banner + §3 rows), `.claude/rules/project/operator-charter-forever.md` (§I pointer), `docs/groww-ref/16-orders-margins-portfolio.md` (banner)
  - Tests: `groww_order_lattice_guard::test_gate4_rule_files_carry_the_lattice_markers`

- [x] Item 1 — Neutral broker order-event seam (reuse `Feed`; includes `EventSource { Poll, Push }`)
  - Files: `crates/common/src/broker_order_events.rs`, `crates/common/src/lib.rs` (`pub mod broker_order_events;`)
  - Tests: `broker_order_events::tests::test_from_groww_every_documented_value_maps_non_unknown`, `test_from_dhan_every_order_status_value_maps_non_unknown`, `test_from_groww_garbage_and_empty_map_unknown_no_panic`, `test_from_dhan_garbage_and_empty_map_unknown_no_panic`, `test_broker_order_status_as_str_all_variants`, `test_is_terminal_classification`, `test_broker_order_event_preserves_raw_status_when_unknown`, `test_event_source_variants_distinct`

- [x] Item 2 — Gate 1: `GrowwOrdersConfig` default-OFF + base.toml block
  - Files: `crates/common/src/config.rs` (`GrowwOrdersConfig` + `ApplicationConfig.groww_orders`), `config/base.toml` (`[groww_orders]`)
  - Tests: `config::tests::test_groww_orders_config_defaults_all_off`, `groww_order_lattice_guard::test_gate1_groww_orders_config_defaults_all_off`, `test_gate1_base_toml_groww_orders_keys_all_false`

- [x] Item 3 — Gate 3: `GROWW_ORDER_LIVE_FIRE` const
  - Files: `crates/common/src/constants.rs`
  - Tests: `constants::tests::test_groww_order_live_fire_is_off`, `groww_order_lattice_guard::test_gate3_live_fire_const_is_false`

- [x] Item 4 — Gate 2: `groww_orders` feature + `oms/groww/mod.rs` stub
  - Files: `crates/trading/src/oms/groww/mod.rs`, `crates/trading/src/oms/mod.rs`, `crates/trading/Cargo.toml`, `crates/common/Cargo.toml`
  - Tests: `groww_order_lattice_guard::test_gate2_groww_orders_feature_present_and_never_default`

- [x] Item 5 — Gate 5: lattice guard (no ungated call site)
  - Files: `crates/common/tests/groww_order_lattice_guard.rs`
  - Tests: `groww_order_lattice_guard::test_gate5_no_ungated_groww_order_side_call_site`

- [~] Item 6 (DEFERRED — area PRs, serial, follow PR-0) — Orders / Smart Orders / Portfolio / Margin / User+Exceptions per §39.3 collision contract, each behind the `groww_orders` feature + all four gates
  - Files: `crates/trading/src/oms/groww/{api_client,smart_orders,portfolio,margin,user}.rs` (one per serial PR); shared `crates/common/src/error_code.rs` + `crates/core/src/notification/events.rs` touched by the build lead only
  - Tests: per-area error-code cross-ref + tag-guard; per-area 22-test-category coverage; each area PR ships its own guard extension keeping live fire locked
  - [x] Item 6a — User + Exceptions (readiness) area PR: `crates/trading/src/oms/groww/user.rs`
    (probe = one bounded in-market GET /v1/margins/detail/user, ≤2 attempts/day;
    verdict GREEN/RED/DEGRADED — RED only on HTTP-proven 401/403; zero new ErrorCode
    variants — reuses Spot1m01FetchDegraded with feed="groww", leg="readiness",
    stage slugs {auth, rate_limited, vendor_reject, http_error, parse, oversize,
    transport, token_read} (+ warn-level payload_parse); /v1/user/detail never
    consumed nor named — schema NOT DOCUMENTED; fire-time const 09:15:30 IST per
    §10 market-hours-only, 08:50 flip documented pending a dated §39 amendment;
    GA classifier pub as the exceptions home for sibling areas; scheduler/SSM/
    Telegram/audit wiring deferred to the build-lead wiring PR)
    - Files: crates/trading/src/oms/groww/user.rs (new), crates/trading/src/oms/groww/mod.rs
      (one `pub mod user;` line + doc comment)
    - Tests: user.rs inline `#[cfg(test)]` — classify_response permutation suite
      (envelope-wins, 401+SUCCESS contradiction, GA005-on-2xx≠auth, 4xx/5xx split,
      dual-shape payload, redaction cap, #1539 GA-sanitizer copy-drift pins),
      2 proptests (classifier + sniffer totality), fire-decision boundaries +
      market-hours-compliance pin, ist_day_number + monotonic day-latch race,
      refusal arms, bounded retry ladder (attempt cap, token re-fetch,
      vendor-reject no-retry, oversize pre-read), operator_summary wording,
      slug stability, 5 source-scan self-ratchets (GET-only, single-URL-literal,
      expose_secret-once, no-unwrap/config/SSM/storage, error!-carries-code)
