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

## Enforcement

- `subscription_planner::build_subscription_plan` uses
  `HashSet<(u32, ExchangeSegment)>`. Guard test
  `test_regression_seen_ids_key_type_is_pair` fails to compile if
  someone regresses the key.
- `universe_builder::build_fno_universe_from_csv` uses
  `HashSet<(SecurityId, char)>` with the CSV segment character.
- `InstrumentRegistry::from_instruments` emits WARN per detected
  cross-segment collision. A follow-up PR will refactor the storage
  key itself to `(SecurityId, ExchangeSegment)`.

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

- Commit `cef501c` (2026-04-17) — fixed planner-level dedup.
- Commit `b46ee8b` (2026-04-17) — fixed universe-builder CSV dedup.
- Follow-up PR pending — registry storage-key refactor (59 call sites).

## Verification

When adding new instrument-handling code, grep for:
```
rg 'HashSet<(u32|SecurityId)>|HashMap<(u32|SecurityId),'
   --glob 'crates/**/src/**.rs'
```

Every hit must either be `(id, segment)` or have a comment
justifying the single-segment assumption.
