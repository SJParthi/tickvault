# Implementation Plan: 2026-08-08 audit MEDIUM follow-ups (codebase-map drift, image digest pin, phantom dep, lossy registry accessors)

**Status:** APPROVED
**Date:** 2026-08-08
**Approved by:** Parthiban (operator) — verbatim, this session, in direct response to a
message presenting this exact bucket as "the only unblocked work, one PR" and asking
for a go/hold decision: *"go"*

Source: the remaining MEDIUM findings from the 2026-08-07 audit chain (PRs #1726 / #1728),
re-verified in source on 2026-08-08 before this plan was written. **Two candidate findings
were DROPPED during that re-verification rather than shipped as padding** — recorded here
so the drop is auditable and nobody "re-finds" them later:

| Dropped candidate | Why it is NOT a defect (Verified 2026-08-08) |
|---|---|
| `obi.rs` labels a level-loop `O(1)` (lines 1, 20, 108, 201) | The loop is `levels.iter().take(min(len, MAX_DEPTH_LEVELS))` with `MAX_DEPTH_LEVELS = 20`. The iteration count is bounded by a **compile-time constant, independent of input size** — that is the definition of O(1). The doc comments already state the bound explicitly ("iterates exactly `min(len, 20)`", "fixed iteration over at most 20 levels"). Unlike the TrueData JSON-scan case (`truedata-feed-scope-2026-07-24.md` §11.3), where cost scales with frame bytes, there is no input-dependent term here. Claim is CORRECT; no change. |
| `app` coverage floor 68.3 is "the weakest / possibly stale" | `quality/crate-coverage-thresholds.toml:22` `app 63.43` is a **dated 2026-06-10 measured baseline comment**, not a current floor. The floor was legitimately ratcheted 63.3 → 68.3 on 2026-07-20 from CI-measured min 68.66 (PR #1694), per the file's own up-only rule. Nothing is stale or contradictory. Raising `app` coverage further is real test-writing work, NOT a doc fix, and is out of scope for this PR. |

## Plan Items

- [x] **Item 1 — CLAUDE.md `crates/storage` table: ALL 7 rows name files that do not exist**
  - Verified: `tick_persistence.rs`, `candle_persistence.rs`, `instrument_persistence.rs`,
    `calendar_persistence.rs`, `materialized_views.rs`, `deep_depth_persistence.rs`,
    `indicator_snapshot_persistence.rs` — 7/7 MISSING from `crates/storage/src/`
    (which holds 40 other modules). Root cause is recorded in the repo itself:
    `quality/crate-coverage-thresholds.toml` documents the 2026-07-17 stage-2 dead-WS
    sweep (PR #1631) deleting `tick_persistence.rs` + siblings.
  - Severity beyond paths: the table advertises a "QuestDB ILP writer (zero-alloc hot
    path)" for ticks. The tick chain is DELETED (live feeds retired 2026-07-13/15), so the
    map misdescribes the ARCHITECTURE, not just filenames — and CLAUDE.md is what every
    session reads at startup per the SESSION PROTOCOL.
  - Files: `CLAUDE.md`
  - Tests: `crates/common/tests/claude_md_codebase_map_guard.rs::every_rs_file_named_in_claude_md_table_exists`
    (NEW ratchet — scans every `` `*.rs` `` in a CLAUDE.md TABLE ROW and fails the build on
    the next drift; prose is deliberately exempt so dated history can keep naming deleted
    modules, and glob patterns like `dhat_*.rs` are skipped — both pinned by their own
    scanner self-tests, `scanner_detects_a_planted_ghost_row` / `scanner_skips_glob_patterns`)

- [x] **Item 2 — QuestDB image is NOT digest-pinned, while CLAUDE.md claims all images are**
  - Verified: `deploy/docker/docker-compose.yml:73` = `questdb/questdb:9.3.5` (tag only),
    while loki (`:231`) and alloy (`:263`) both carry `@sha256:`. `CLAUDE.md:380` asserts
    *"All images pinned with SHA256 digest"* — that assertion is FALSE today. A document
    asserting a supply-chain control that is not in place is worse than an unpinned tag.
  - Digest resolved live from Docker Hub (NOT invented):
    `sha256:22ad030544f45a396c743124c928ce33de11666c104f63f69ce4e66e07e7b968`
    — `Docker-Content-Digest` header, `mediaType: application/vnd.oci.image.index.v1+json`,
    round-tripped by digest (http 200), index carries BOTH `linux/amd64` and `linux/arm64`.
    Pinning the multi-arch INDEX (not a per-arch manifest) is required because dev is
    Mac/arm64 and prod is ARM Graviton `t4g` — a single-arch pin would break one of them.
  - Second call site: `scripts/ensure-questdb.sh:34` pins the same bare tag. Pinning only
    compose would leave the local bootstrap path drifting, so both move together.
  - Files: `deploy/docker/docker-compose.yml`, `scripts/ensure-questdb.sh`
  - Tests: `crates/storage/tests/docker_image_digest_guard.rs::every_compose_image_is_digest_pinned`
    (NEW ratchet — makes the CLAUDE.md claim mechanically true from now on)

- [x] **Item 3 — `papaya` is an UNUSED dependency in `crates/core`, masked from tooling**
  - Verified: `crates/core/Cargo.toml:21` declares `papaya = { workspace = true }` in
    `[dependencies]`. There is ZERO `papaya` reference in `crates/core/src`, `crates/core/tests`,
    or `crates/core/benches`. It is simultaneously listed in core's cargo-machete allowlist
    (`crates/core/Cargo.toml:110` `ignored = [... "papaya" ...]`), so unused-dep tooling can
    never surface it.
  - Honest scope: `papaya` IS genuinely used in `crates/trading`
    (`src/in_mem/day_ohlc_tracker.rs:48` `use papaya::HashMap as PapayaHashMap`). This item
    removes it ONLY from `core`. The workspace pin (`Cargo.toml:39` `papaya = "0.2.4"`) and
    trading's dependency both STAY.
  - Files: `crates/core/Cargo.toml` (drop the dep line + drop `"papaya"` from `ignored`)
  - Tests: existing `cargo build -p tickvault-core` + `cargo test -p tickvault-core` prove
    core never needed it; `cargo machete` no longer needs the allowlist entry

- [x] **Item 4 — Legacy single-segment registry map + its two collision-LOSSY accessors have zero production callers**
  - Verified: `InstrumentRegistry` carries BOTH `by_composite: HashMap<(SecurityId,
    ExchangeSegment), _>` (the I-P1-11 source of truth) and a legacy
    `instruments: HashMap<SecurityId, _>`. The legacy map is read by exactly two methods —
    `get()` (`:357`) and `contains()` (`:391`) — whose own doc comment admits they return
    "whichever entry won the insert race" on a cross-segment collision. Call-site sweep of
    `crates/*/src`: ZERO production callers; the only callers are unit tests inside
    `instrument_registry.rs` itself (`:730`, `:731`, `:773`, `:774`, `:775`, `:1016`).
  - Why it matters: this is the exact footgun `security-id-uniqueness.md` exists to prevent
    (FINNIFTY `IDX_I` id=27 vs an `NSE_EQ` id=27), kept alive as public API for tests only.
    The banned-pattern scanner already rejects new `.registry.get(id)` call sites — this
    removes the trap itself instead of guarding it forever.
  - MUST PRESERVE (load-bearing, ratcheted): `cross_segment_collisions()` and
    `collision_pairs` — the I-P1-11 gauge + operator diagnostics. Collision detection
    currently derives from duplicate inserts into the legacy map, so it is re-expressed
    over a cold-path construction-time `HashSet<SecurityId>` with byte-identical counting
    and identical `(id, losing_segment, winning_segment)` tuples.
  - **SCOPE CORRECTION (during implementation, 2026-08-08 — recorded per
    plan-enforcement rule 5, not silently absorbed).** The initial sweep covered
    `crates/*/src` only and concluded "tests in the same file". A wider sweep across
    `tests/`, `benches/`, and `examples/` found TWO more consumers, so the blast radius is
    larger than this item originally advertised:
    - `crates/core/tests/dhat_instrument_registry.rs` — a DHAT **zero-allocation** test
      whose entire subject was `registry.get()`.
    - `crates/core/benches/instrument_registry.rs` — a Criterion **benchmark with a budget
      contract**: `registry_get = 50` in `quality/benchmark-budgets.toml`, enforced by
      `scripts/bench-gate.sh` and documented in CLAUDE.md's BENCHMARK BUDGETS table.
    - `crates/core/tests/load_stress.rs` — one 8-thread lookup loop.

    All three were MIGRATED to `get_with_segment`, not deleted. This is strictly better: the
    50 ns budget was guarding a method with **zero production callers**, and now guards the
    composite lookup production actually performs. **The budget contract is UNCHANGED** —
    the Criterion IDs stay byte-identical (`registry/get_hit`, `registry/get_miss`,
    `registry/contains_hit`) so the `registry_get` key keeps matching and historical
    Criterion baselines stay comparable. Only the method body called inside each bench
    changed.
  - Rider (same line, same drift class): `quality/benchmark-budgets.toml:21` credited
    "papaya O(1) lookup" for what is a plain `std::HashMap`. Comment corrected; budget
    VALUE untouched at 50.
  - Files: `crates/common/src/instrument_registry.rs`,
    `crates/core/tests/dhat_instrument_registry.rs`,
    `crates/core/benches/instrument_registry.rs`, `crates/core/tests/load_stress.rs`,
    `quality/benchmark-budgets.toml`
  - Tests: existing registry tests migrated to `get_with_segment()`/`contains_with_segment()`;
    `crates/common/tests/` I-P1-11 collision assertions stay green unchanged.
    `crates/common/` change ⇒ escalates to `cargo test --workspace` per `testing-scope.md`.
  - NEW ratchet: `crates/common/tests/instrument_registry_composite_only_guard.rs`
    (4 tests — lossy accessors stay deleted · segment-aware API still present ·
    collision reporting survived · workspace sweep for id-only call sites)

## Design

Four independent, reversible cleanups that make three documented-but-false statements true
and delete one latent-footgun code path. Ordering is deliberate: Items 1–3 are
single-file/no-logic and land first so a failure in Item 4 (the only one touching Rust
logic) cannot block them. Each item ships with a ratchet so the same drift cannot return —
per `z-plus-defense-doctrine.md`, a fix without a build-failing guard is a wish.

Item 4 is the only item with behavioural surface. It is a pure DELETION of unreachable
public API plus a like-for-like re-expression of collision counting; `by_composite`,
`get_with_segment`, `iter()`, `len()` (which reads `total_count`, not either map),
`by_exchange_segment()`, and the category counts are untouched.

## Edge Cases

| Case | Handling |
|---|---|
| CLAUDE.md later renames a real module | Item 1's ratchet scans every `` `*.rs` `` token, so any future rename that drops a file fails the build |
| QuestDB publishes a new 9.3.5 build under the same tag | Digest pin means the compose file keeps resolving the EXACT verified image; an upstream retag can no longer silently change prod |
| Mac/arm64 dev vs ARM Graviton prod | Multi-arch INDEX digest verified to carry both `linux/amd64` and `linux/arm64`; docker resolves per-platform |
| `ensure-questdb.sh` and compose drift apart | Both pinned in the same commit; the ratchet covers compose, and the script references the same literal |
| A cross-segment collision occurs after Item 4 | Detection preserved via construction-time `HashSet`; both entries remain addressable through `by_composite` exactly as before |
| A future caller wants id-only lookup | The lossy method no longer exists, so they are forced to supply a segment — the I-P1-11 outcome we want |
| `papaya` later needed in core | Re-add the one-line workspace dep; the pin stays in the root `Cargo.toml` |

## Failure Modes

| Failure | Detection | Consequence if unfixed |
|---|---|---|
| Digest typo / wrong digest | `docker compose pull` fails loudly with a manifest mismatch; CI Build & Verify + the new ratchet | QuestDB will not start — LOUD, never silent-wrong data |
| Collision counting regression in Item 4 | Existing I-P1-11 ratchet tests assert counts and pairs | `tv_instrument_registry_cross_segment_collisions` under-reports; caught pre-merge |
| Removing `papaya` breaks a hidden core usage | `cargo build -p tickvault-core` fails at compile time | Impossible to merge broken — compile-time, not runtime |
| Test migration in Item 4 hides a real assertion change | Assertions kept semantically identical; only the accessor gains an explicit segment | Weakened coverage; reviewed in the diff |

## Test Plan

1. `cargo test -p tickvault-common` — registry + the new CLAUDE.md map guard
2. `cargo test -p tickvault-core` — proves core never used `papaya`
3. `cargo test -p tickvault-storage` — the new docker digest guard
4. `cargo test --workspace` — MANDATORY escalation: `crates/common/` changed (`testing-scope.md`)
5. `cargo fmt --check` + `cargo clippy --workspace -- -D warnings`
6. `bash .claude/hooks/banned-pattern-scanner.sh` + `pub-fn-test-guard.sh` + `pub-fn-wiring-guard.sh`
7. Bite-proof each new ratchet: re-introduce the defect, confirm the test FAILS, revert
   (a guard never observed failing is not a guard)

## Rollback

Every item is independently revertable by a single-file `git revert`; there is no data
migration, no schema change, and no config semantics change. Item 2 is the only item that
touches a runtime artifact — reverting restores the bare tag, and because the digest
resolves the identical image, a revert is behaviourally inert. Item 4 restores two
unreachable methods. No operator action, no redeploy sequencing, no state to unwind.

## Observability

No new counters, gauges, alarms, Telegram events, or log lines — this PR removes false
claims and dead code and adds build-time guards. Existing signals are explicitly
PRESERVED: `tv_instrument_registry_cross_segment_collisions` and
`tv_instrument_registry_total_entries` keep their exact semantics (Item 4's stated
constraint), and `cross_segment_collisions()` remains the operator-facing accessor read by
`/health` and `make doctor`. Observability delta is therefore zero by design; the new
signal is three build-failing ratchets.

## Per-Item Guarantee Matrix

See `.claude/rules/project/per-wave-guarantee-matrix.md` for the canonical 15-row
"100% code coverage" matrix and the 7-row "Zero ticks lost" resilience matrix. Both are
cross-referenced rather than duplicated, per that file's stated allowance. Per-item
specifics for this PR:

| Row | This PR |
|---|---|
| 100% code coverage | 3 new ratchet tests; no production logic added, one dead path removed (coverage ratio improves or holds) |
| Audit coverage | N/A — no new typed event; no audit table touched |
| Testing coverage | unit + source-scan ratchets; workspace escalation for `crates/common/` |
| Code performance | N/A — no hot-path change. Item 4 deletes a cold-path map; `by_composite` lookup is untouched |
| Monitoring / logging / alerting | N/A — zero new signals; existing I-P1-11 gauge semantics explicitly preserved |
| Security hardening | Item 2 makes a documented supply-chain control REAL (digest pin); Item 3 removes an unused compiled-in dependency |
| Uniqueness + dedup | Item 4 STRENGTHENS I-P1-11: the collision-lossy id-only lookup ceases to exist |
| Extreme check | each of the 3 ratchets bite-proven by re-introducing the defect |

### Resilience rows (7-row matrix)

Zero ticks lost: UNAFFECTED — no tick path, WAL, ring, spill, or DLQ code is touched (the
tick chain no longer exists; live market-data feeds are retired). WS/QuestDB resilience,
O(1) hot path, and the seal chain are all untouched. Item 2 strengthens QuestDB startup
determinism by pinning the exact verified image.

## Honest envelope

100% inside the tested envelope, with ratcheted regression coverage: three build-failing
guards (CLAUDE.md `.rs` existence, compose digest pinning, plus the pre-existing I-P1-11
collision assertions retained over the re-expressed counting path). NOT claimed: that
CLAUDE.md's PROSE is now fully accurate — this PR fixes the `crates/storage` table and the
image-pinning sentence, both mechanically verified, and does not re-audit every other
narrative claim in the file. NOT claimed: that `papaya` removal changes any runtime
behaviour — core never referenced it, so the change is compile-surface only. NOT claimed:
any live-runtime verification — the box is stopped and `main` (`df9ea5ce`) is itself not
yet deployed (its 08:45 IST cron is pending), so every claim here is source- and
CI-verified, never live-verified.
