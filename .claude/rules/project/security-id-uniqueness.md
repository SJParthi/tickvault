# security_id Uniqueness Enforcement (I-P1-11, 2026-04-17)

> **Authority:** CLAUDE.md > this file.
> **Scope:** Every collection, lookup table, dedup set, and registry
> that stores or looks up instruments by Dhan `security_id`.
> **Trigger:** This rule is always loaded.

## The rule — one line

**`security_id` alone is NOT unique. The only unique instrument key is
`(security_id, exchange_segment)`.**

## The bug class this rule prevents

Dhan's instrument master CSV reuses the same numeric `security_id`
across different `ExchangeSegment` values. Concrete example spotted
live on 2026-04-17:

- FINNIFTY index value: `security_id = 27`, segment = `IDX_I`
- Some NSE_EQ instrument in the same CSV: `security_id = 27`, segment = `NSE_EQ`

These are two **logically distinct instruments**. Any code that
deduplicates or looks up by `security_id` alone will silently drop
one of them, leading to:
- Missing WebSocket subscriptions (live symptom: FINNIFTY IDX_I not
  subscribed → depth ATM selector has no spot price → depth-20 for
  FINNIFTY silently missing from Grafana dashboard for hours).
- Incorrect tick enrichment (tick from segment A looked up and
  enriched with metadata from segment B).
- Silent data loss in the instrument pipeline.

## Mechanical rules

1. **`HashSet<u32>` / `HashSet<SecurityId>` are BANNED** for instrument
   dedup when there is ANY chance the collection spans multiple
   segments. Replace with `HashSet<(u32, ExchangeSegment)>` or
   `HashSet<(SecurityId, char)>` where `char` is the CSV segment code.

2. **`HashMap<u32, _>` / `HashMap<SecurityId, _>`** keyed on
   `security_id` alone are ALLOWED only when the caller can prove
   every value in the map belongs to a single known segment (e.g.
   `FnoUniverse.derivative_contracts` is safe because every
   derivative is NSE_FNO). Add a doc-comment at the declaration
   stating WHY the single-segment assumption holds.

3. **Every lookup by `security_id`** must either:
   - (a) be accompanied by a segment check, OR
   - (b) be inside a code path where the segment is guaranteed by
     construction (and the caller commented WHY).

4. **Tick-header lookups** always have the `exchange_segment_code`
   in the 8-byte header. Use `(security_id, exchange_segment_code)`
   as the lookup key, not `security_id` alone.

5. **New subscription-plan / registry code** MUST use
   `(security_id, ExchangeSegment)` for dedup. Regression tests
   `subscription_planner::tests::test_regression_*` enforce this.

## Enforcement (7 layers, all merged 2026-04-17)

1. **Planner dedup** — `subscription_planner::build_subscription_plan`
   uses `HashSet<(u32, ExchangeSegment)>`. Compile-time guard test
   `test_regression_seen_ids_key_type_is_pair` fails if regressed.
2. **CSV-parse dedup** — `universe_builder::build_fno_universe_from_csv`
   uses `HashSet<(SecurityId, char)>` with the CSV segment character.
3. **InstrumentRegistry composite index** — `by_composite:
   HashMap<(SecurityId, ExchangeSegment), _>`. `iter()`,
   `by_exchange_segment()`, `len()`, category counts and the
   `underlying_symbol_to_security_id` map all iterate this map so
   downstream consumers (subscribe builder, Greeks, dashboards) see
   BOTH colliding entries.
4. **Storage DEDUP keys** — every `DEDUP_KEY_*` constant that
   mentions `security_id` also mentions `segment` / `exchange_segment`
   / `exchange`. Meta-guard
   `crates/storage/tests/dedup_segment_meta_guard.rs` scans the
   storage crate and fails the build on regression.
5. **Banned-pattern hook** — `.claude/hooks/banned-pattern-scanner.sh`
   category 5 rejects new `HashSet<u32>`, `HashSet<SecurityId>`,
   `HashMap<u32, _>`, `HashMap<SecurityId, _>`, `.registry.get(id)`,
   `.registry.contains(id)` inside instrument paths without an
   `// APPROVED:` comment on the immediately preceding line.
6. **FnoUniverse runtime detection** — `universe_builder.rs` emits
   `tv_fno_universe_derivative_collisions_total` when NSE_FNO/BSE_FNO
   derivative ids collide.
7. **Prometheus visibility** — `InstrumentRegistry::cross_segment_collisions()`
   getter exposes the count. `subscription_planner` emits gauges
   `tv_instrument_registry_cross_segment_collisions` and
   `tv_instrument_registry_total_entries`. Construction log is at
   `info!` level (downgraded 2026-04-19): both entries are stored
   safely in `by_composite` and every production caller uses
   `get_with_segment(id, segment)`, so this is not a data-loss event
   and does not warrant a Telegram page. Operators see the count via
   `/health` and `make doctor` (which read the gauge), not via a
   per-boot alert. Ratchet: `test_cross_segment_log_level_is_info_not_error`
   in `instrument_registry.rs` blocks regressions back to `error!`.

## Dashboard enforcement

- Grafana dashboards must not use `count_distinct(security_id)` on
  cross-segment tables (ticks, market_depth, deep_market_depth,
  candles_*, historical_candles) without also qualifying by
  `segment`. Enforced by
  `crates/storage/tests/grafana_dashboard_snapshot_filter_guard.rs::
  dashboard_distinct_security_id_must_include_segment`.
- The offending query in `market-data.json` was rewritten to
  `SELECT count(*) AS total FROM (SELECT DISTINCT security_id,
  segment FROM ticks)`.

## Where this rule fires (auto-loaded paths)

- `crates/core/src/instrument/subscription_planner.rs`
- `crates/core/src/instrument/universe_builder.rs`
- `crates/common/src/instrument_registry.rs`
- `crates/common/src/instrument_types.rs`
- `crates/core/src/pipeline/tick_processor.rs` (tick enrichment lookups)
- `crates/trading/src/greeks/aggregator.rs` (option state)
- Any file containing `HashSet<u32>`, `HashSet<SecurityId>`,
  `HashMap<u32,`, `HashMap<SecurityId,`.

## Historical context

All commits on branch `claude/fix-duplicate-timestamps-3M2o0` / PR #273:

- `cef501c` (2026-04-17) — planner-level dedup `HashSet<(u32, ExchangeSegment)>`
- `b46ee8b` (2026-04-17) — universe-builder CSV dedup `HashSet<(SecurityId, char)>`
- `d8bfce5` (2026-04-17) — `InstrumentRegistry` composite index +
  `iter()`/`by_exchange_segment()`/`len()`/category counts all
  iterate `by_composite` + 5 unit regression tests
- `c7397b3` (2026-04-17) — 5 production `registry.get(id)` sites
  migrated to `get_with_segment(id, segment)`
- `e05e66c` (2026-04-17) — banned-pattern hook category 5
- `49a6b5a` (2026-04-17) — 4 integration regression tests
- `f28378b` (2026-04-17) — dashboard guard + `market-data.json` fix
- `6f5e5c3` (2026-04-17) — storage DEDUP keys include segment + meta-guard
- `01bd833` (2026-04-17) — FnoUniverse runtime collision detection
- `d049bd6` (2026-04-17) — Prometheus gauges +
  `cross_segment_collisions()` getter

## Verification

When adding new instrument-handling code, grep for:
```
rg 'HashSet<(u32|SecurityId)>|HashMap<(u32|SecurityId),'
   --glob 'crates/**/src/**.rs'
```

Every hit must either be `(id, segment)` or have a comment
justifying the single-segment assumption.
