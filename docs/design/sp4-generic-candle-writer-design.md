# SP4 — GenericCandle1mWriter(feed) — Design

## §0. The honest starting fact (read the code first)

SP4's plan text ("merge the two writers") was written BEFORE SP3b (#1180) landed.
The merge it describes is **already structurally complete**:

- The Groww-only candle writer (`GrowwCandle1mWriter` / `GrowwCandle1mRow` / `append_row`)
  was **already DELETED** in the one-candle-engine convergence. `groww_candle_persistence.rs`
  now retains ONLY the DDL bootstrap `ensure_groww_candles_1m_table` (delegates to the shared
  candle-table DDL).
- Both feeds' seals already flow through ONE shared writer: `MultiTfAggregator` (per-feed
  `FeedStrategy`) → `BufferedSeal { feed: Feed, .. }` → `mpsc::Sender<BufferedSeal>` →
  `SealWriterRunner` (owns exactly ONE `ShadowCandleWriter`) → `ShadowCandleWriter::append_seal`.
- `ShadowCandleWriter::append_seal` already stamps the `feed` SYMBOL **from the seal**
  (`row.feed = seal.feed.as_str()`), NOT a hardcoded constant. The shared candle tables
  `candles_<tf>` already carry `feed` in `DEDUP_KEY_CANDLES = "ts, security_id, segment, feed"`.
- Tests already prove it: `test_append_seal_stamps_feed_from_seal_not_hardcoded` (a `Feed::Groww`
  seal stamps `feed=groww`, never `feed=dhan`), `test_append_seal_stamps_feed_dhan_in_wire_bytes`,
  and the dedup meta-guard `per_feed_market_data_dedup_keys_must_include_feed`.

So `ShadowCandleWriter` IS the merged, feed-parameterized writer — it is parameterized
**per-seal** (the seal carries its feed), which is strictly MORE general than "constructed
with one feed", because the SAME shared seal-writer chain interleaves Dhan + Groww seals
on one ILP connection.

## §1. What SP4 still delivers (the named contract, additive, behavior-preserving)

The plan asks for a `GenericCandle1mWriter(feed)` API — "constructed with a `Feed`".
We deliver that as a thin, explicit **feed-pinned facade** over the existing
per-seal writer, WITHOUT changing the shared seal-writer chain or any wire bytes:

```
GenericCandle1mWriter {
    feed: Feed,                 // the feed this writer instance is pinned to
    inner: ShadowCandleWriter,  // the existing per-seal feed-parameterized writer
}
```

API (mirrors `ShadowCandleWriter`, but feed is bound at construction):

| Method | Behavior |
|--------|----------|
| `new(config, feed) -> Result<Self>` | lazy-connect (same as ShadowCandleWriter::new) + remember `feed` |
| `for_test(feed) -> Self` | disconnected test writer pinned to `feed` |
| `feed(&self) -> Feed` | the pinned feed |
| `append_candle(&mut self, seal) -> Result<()>` | **asserts** `seal.feed == self.feed` (debug) and delegates to inner.append_seal — preserves exact bytes, tables, DEDUP, flush semantics |
| `flush / is_connected / pending_count / buffer_*` | delegate to inner verbatim |

The feed-pin is a **defensive invariant**, not a behavior change: it guarantees a
`GenericCandle1mWriter(Feed::Dhan)` only ever writes Dhan seals, and a
`GenericCandle1mWriter(Feed::Groww)` only Groww — which is exactly the plan's
"one feed-parameterized writer" contract made explicit and type-checked. The
production shared seal-writer chain keeps using the per-seal `ShadowCandleWriter`
(it MUST interleave both feeds on one connection — pinning it to one feed would
break the shared-chain design), so SP4 is **additive**: it names + locks the
contract and gives a single-feed handle for any future single-feed wiring,
while not disturbing the live multi-feed seal chain.

## §2. How the two old writers are "replaced"

| Old writer | Status |
|-----------|--------|
| `GrowwCandle1mWriter` (Groww 1m-only) | already DELETED (SP3b #1180); Groww uses the shared chain |
| `ShadowCandleWriter` (Dhan + now-both seal writer) | KEPT as the per-seal engine; `GenericCandle1mWriter` is the feed-pinned facade over it |

No call-site migration is needed for the live path (the shared `SealWriterRunner`
already serves both feeds). `GenericCandle1mWriter` is the named, feed-parameterized
public contract the plan named; it is exercised by SP4's tests proving a Dhan candle
and a Groww candle each persist (stamp the correct `feed`) through the SAME generic
writer type with the existing DEDUP keys.

## §3. The `feed` label + DEDUP (unchanged, just re-proven)

- `feed` SYMBOL written from the seal: `feed='dhan'` / `feed='groww'`.
- `candles_<tf>` DEDUP = `(ts, security_id, segment, feed)` (`DEDUP_KEY_CANDLES`).
- `per_feed_market_data_dedup_keys_must_include_feed` stays GREEN (we touch no DEDUP key).
- Wire bytes / tables / OHLCV columns / pct columns / flush-ring-spill semantics: IDENTICAL.

## §4. Tests SP4 adds

In `generic_candle_writer.rs` (new module wrapping `shadow_candle_writer`):

1. `test_generic_writer_pins_feed` — `GenericCandle1mWriter::for_test(Feed::Dhan).feed() == Dhan`.
2. `test_generic_writer_dhan_seal_stamps_feed_dhan` — a Dhan seal → `feed=dhan` in wire bytes.
3. `test_generic_writer_groww_seal_stamps_feed_groww` — a Groww seal → `feed=groww` in wire bytes,
   via the SAME generic writer type (different feed instance).
4. `test_generic_writer_both_feeds_one_writer_type` — both rows persist through `GenericCandle1mWriter`,
   each with the correct `feed`, same `candles_1m` table.
5. `test_generic_writer_delegates_flush_disconnected` — flush err when disconnected (behavior preserved).
6. `test_generic_writer_rejects_mismatched_feed` — a `GenericCandle1mWriter(Dhan)` rejects a Groww seal
   (defensive feed-pin; returns Err, never silently writes the wrong feed label).

## §5. Z+ / guarantee mapping

- O(1): facade is a thin struct + delegation; no hot-path alloc added (DHAT path unchanged — the
  live seal chain still uses `ShadowCandleWriter` directly).
- Uniqueness/dedup: `feed` stays in `DEDUP_KEY_CANDLES`; meta-guard green.
- Behavior preserved: Dhan candle bytes/tables/flush identical; no live call-site rewired.
- Honest envelope: SP4 names the already-converged writer as `GenericCandle1mWriter(feed)` +
  adds a type-checked feed-pin + tests proving both feeds persist via one generic writer type.
  It does NOT claim to "newly merge" code that SP3b already merged — it locks the contract.
