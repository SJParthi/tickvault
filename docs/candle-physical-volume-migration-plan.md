# Physical candle volume migration: proposed, not implemented

The current 25-column candle schema retains gross `volume` and a nullable
`net_volume` cache whose meaning depends on `volume_basis`. Top Volume V3
already exposes the signed cache as `volume` in its API/views. That projection
does not remove the physical column. This document preserves the required
migration mapping; it is not an executable migration or a deployment claim.

| New physical field | Mapping and invariant |
|---|---|
| `gross_volume LONG` | Preserve existing gross quantity, including NULL/uncertainty where historical evidence is absent; never derive it from signed volume |
| `volume LONG` | Nullable whole-bar signed quantity from an explicit valid V3 cache; legacy/unknown sign remains NULL |
| `volume_basis` | Retain exact provenance; never relabel old tick-rule sums as V3 |
| Other 22 columns | Preserve identity, timestamp convention, OHLC, metadata, quality, revision and pending book quantities |

The target remains 25 columns. A flat candle may have gross 500 and signed
volume 0, so `abs(volume)` cannot reconstruct gross. Source gross saturation
or corrupt values cannot be repaired by renaming fields; retain their quality
flags and original durable evidence and investigate them separately.

The V3 sign uses the predecessor close frozen when that candle opened. Old
spill formats and historical rows do not necessarily retain that baseline.
Recomputing with the latest database predecessor close can manufacture a
different answer after corrections. Existing tick-rule values must remain
available in verified legacy storage, without being presented as current V3.

## Data-preserving cutover

Create a separate physical schema generation, preserving partitioning and
`(ts, security_id, segment, feed)` deduplication. Fence every live/REST/replay
writer and maintenance job. Await applied WAL, preserve source schemas and
full original payloads in verified legacy storage/backup, and record a durable
step manifest. Copy explicit field mappings; only explicit valid V3 caches
populate signed `volume`. Legacy rows preserve gross and their other known
facts while signed volume stays NULL.

Verify key/row counts, full typed payload equality, gross totals, NULLs and
applied WAL before switching canonical names and recreating dependent views.
Do not assume the multi-table operation is atomic. Every step needs restart
reconciliation. An old binary must be blocked: it would write gross into the
new signed column and can recreate `net_volume` through ILP/DDL self-heal.
Rollback after new writes needs a verified reverse mapping or a forward fix;
retain all post-cutover source data.

## Boundaries that must change together

| Boundary | Required treatment |
|---|---|
| `shadow_persistence.rs` | New columns and mandatory schema readiness; remove `ADD net_volume` self-heal |
| `shadow_seal_columns.rs` | Explicit gross/signed row fields and legacy-preserving conversion |
| `shadow_candle_writer.rs` | Emit gross under `gross_volume`, signed under `volume`; no physical `net_volume` writes |
| `seal_recovery_guard.rs` | New 25-field equality and migration-aware legacy witness; retain writer exclusion, WAL barriers and conflicts |
| Seal task/runner/loop/absorption | Correct destination generation and complete durable result before confirmation |
| `seal_spill.rs` / `seal_dlq.rs` | Preserve existing binary/JSON meanings and originals; v4 already carries gross plus V3 signed cache |
| `candle_top_volume_views.rs` | Read signed `c.volume`, validate against `c.gross_volume`, retain exact sorting and quality gates |
| `console_views.rs` | Remove old physical names from the current candle projection and expose basis clearly |
| `tf_consistency_boot.rs` | Sum/compare gross in both 1m and higher-timeframe queries |
| `dhan_live_crossverify.rs` | Compare broker traded quantity against gross, never against signed volume |
| `rest_candle_fold.rs` | Preserve gross; missing sign baseline remains unclassified |
| RAM candle/Top Volume bridge | Keep gross state separate from signed cache; V3 API already uses signed `volume` |
| Retention/archive/restore and operator SQL | Preserve schema generation and keep backups out of active cleanup |
| Fixtures and external SQL/API consumers | Migrate and verify together; repository search does not inventory external users |

Legacy recovery cannot simply ignore the removed cache field. The current
guard compares all original payload fields, with a narrow unchanged legacy
NULL-extension exception. Use retained legacy rows as equality witnesses or
a separately identified legacy recovery destination. An old original can be
confirmed only after its full historical payload is durably preserved, not
after writing a lossy gross/NULL projection. Conflicts remain pending.

## Retiring unsupported frames without deleting recovery evidence

Keep candles `1s,3s,5s,1m,3m,5m,10m,15m,30m,60m`; keep primary Top Volume
`1s,3s,5s,1m`. The retired durable identities represent `1d,2s,4s,6s..15s,30s,2m`.
Inventory actual tables, ordinary views, materialized views and dependencies;
apply an exact reviewed name list, never a prefix deletion sweep.

Ordinary views have definitions to preserve. Tables and materialized views
can hold unique history and require verified preservation before removal.
Renaming removes an active name but does not physically remove the retained
table. Actual dropping requires a verified archive and restore check first.
Keep raw ticks, directory authority and unconfirmed WAL/spill/DLQ bytes.
Retired wire IDs must never be reused. Current recovery defers an entire mixed
file containing a retired record, including its active siblings; table cleanup
must not delete that file or falsely confirm its supported prefix.

## Required release evidence

The current passed QuestDB fixtures cover the existing schema, not this plan.
New isolated-engine tests must prove mixed legacy/current rows, positive-gross
flat candles, negative signed values, unknown provenance, metadata NULLs,
partial cutover/restart, conflicting legacy replay, old-binary startup refusal,
gross cross-verification and timeframe retirement/restore. Measure migration
capacity on the actual AWS host and reserve sufficient disk before copying.
Copying/history verification scales with data size; it is not O(1).
