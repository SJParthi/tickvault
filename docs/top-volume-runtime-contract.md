# Top Volume runtime contract

## Current signed-bar contract — September 15, 2026

The active candidate now uses **`signed_bar_volume_vs_one_lot_v3`**: the existing candle magnitude signed by close versus the frozen previous same-timeframe close. Independent signed-tick accumulation is removed. Top Volume v3 exposes one signed `volume`. Physical storage keeps compatibility fields with explicit basis provenance.

The authoritative current quantity, API, storage, edge-case and measured AWS comparison is [signed-bar-volume-2026-09-15.md](signed-bar-volume-2026-09-15.md). This is local source only: Rust validation, database migration and deployment have not completed. The opening baseline and receipt-clock discrepancies documented there are not fixed by a sign change.

## Historical v2 source contract

**The remainder records the preceding v2 candidate. Its tick-rule quantity, v2 metric identifier, API quantity names and recovery format are superseded by the v3 contract above. Its historical validation numbers are not evidence for v3.**

Updated 2026-09-15 for the candle-derived implementation in the current working tree. **This is a source contract, not a completed deployment or a guarantee that every market tick is available. Current implementation latencies are unmeasured.** A source-matched Rust, QuestDB and AWS validation campaign must be recorded before promoting its results to release evidence. Earlier test counts and timing numbers belong to the historical implementation identified at the end of this document.

## The candle and Top Volume relationship

Top Volume now reuses the candle's signed `net_volume` and pinned `lot_size`. It does not run a second cumulative-volume calculation with a separate timer baseline.

The current requested scope is **ten candle frames and four Top Volume frames**. Top Volume uses only `1s`, `3s`, `5s` and `1m`; longer candle frames continue to be folded and stored without live ranking or newly managed Top Volume views.

| Surface | Quantity source | Purpose | Database read? |
|---|---|---|---|
| Live candle state | Accepted observations folded once into each candle bucket | Updating current and retained amended candles | No |
| Live Top Volume winner | That candle owner's `CandleVolumeUpdate`: signed net, lot size, timestamp, quality and revision | Reading a prepared winner after explicit data checks | No |
| Live Top Volume board | Maintained exact index over canonical candle updates | Inspecting a bounded page of the latest prepared ranking | No |
| `candles_<tf>` | Sealed candle state, persisted asynchronously | Historical candle quantities, OHLC and audit fields | Yes |
| `top_volume_<tf>` | Corresponding `candles_<tf>` row | Comparing historical candle net volume in lots | Yes |
| `top_volume_legacy_<tf>` | Retained old physical `top_volume` table | Inspecting the previous unsigned timer metric with explicit provenance | Yes |

For historical display, selecting the candle table directly is the intended design. For a live decision, reading the prepared RAM result avoids database admission, flush, acknowledgment and a query. The default strict policy still waits for a qualified local candle closure. These are two consumers of one candle quantity, not two definitions of net volume. RAM and database visibility can lag each other; reconciliation must match instrument identity, bucket, definition and candle revision.

The primary metric is **`signed_estimated_net_vs_one_lot_v2`**: a signed tick-rule estimate relative to one lot. It is neither previous-candle growth nor a verified exchange aggressor-side trade total.

## Quantities, lot size and percentage

Let `G` be candle gross `volume`, `Q` be candle signed `net_volume`, and `L` be the candle's positive pinned `lot_size`.

| Value | Formula or source | Meaning |
|---|---|---|
| Gross volume | `G`, from the shared accepted cumulative-counter delta | Total inferred incremental units assigned to this candle |
| Signed net volume | `Q`, from the candle's tick-rule classification | Inferred positive-side units minus inferred negative-side units |
| Per-lot quantity | `L`, from the selected contract definition, pinned when the candle opens | Units used for one lot under this candle's definition |
| Gross lots | `G / L` | Total inferred activity in lots; cannot be negative |
| Signed net lots | `Q / L` | Signed balance in lots; can be negative or fractional |
| Net volume percentage | `100 × (Q / L − 1)` | Signed net relative to one lot; the primary descending score |

The instrument-master-derived `ContractUnderlyingMap` supplies lot size, underlying and family from a coherent selected-universe publication. The candle pins those values and its instrument definition version. Later metadata changes do not rewrite an old candle's denominator. Provider volume units and instrument lot-size units must be compatible; arithmetic alone does not prove that source-specific premise.

With `L = 100`, the order is:

| Signed candle net `Q` | Signed net lots | Percentage versus one lot | Relative position |
|---:|---:|---:|---|
| +600 | +6 | +500% | Highest of these examples |
| +100 | +1 | 0% | Below +600 |
| 0, genuinely classified | 0 | −100% | Below every positive net |
| −100 | −1 | −200% | Below zero |
| −600 | −6 | −700% | Lowest eligible example |
| NULL / unclassified | Unavailable | Unavailable | Excluded from live ordered rows; retained diagnostically in SQL |

Gross volume alone does not change this order. With equal lot sizes, `G = 10,000, Q = −100` ranks below `G = 600, Q = +600`. The code does not take `abs(Q)`. A zero percentage means one net lot; it does not mean zero net volume.

RAM compares `Q1 × L2` with `Q2 × L1` using wide integer arithmetic. SQL uses signed integer quotients plus 31/31/2-bit remainder limbs over the admitted signed-LONG/positive-u32 domain. Neither sorts on rounded display percentages. Truly equal ratios use security ID and the segment's defined ordinal as stable identity tie-breaks. SQL orders bucket timestamp descending and family ascending before the within-bucket score, with feed included in final identity order. A caller can override view order with its own `ORDER BY`.

JSON exposes the exact rational percentage as `score_numerator = 100 × (Q − L)` and `score_denominator = L`. Display lots and percentages are decimal strings truncated toward zero to three decimal places. SQL DOUBLE display values can round; they do not control the exact order.

## Function architecture and source ownership

| Stage / function | Input → result | Responsibility |
|---|---|---|
| `ParsedTick::volume_observation` | Presence and counter → `Missing` or `Cumulative` | A packet without a volume field is not a zero-volume observation. |
| `VolumeCounter::observe` | Session and observation → increment plus quality | Shared counter policy; first observation seeds a baseline. A lower same-session counter preserves the accepted axis and marks ambiguity. |
| `ContractUnderlyingMap::versioned_snapshot` | Selected definitions → coherent universe and definition versions | Cold preparation. The ranking population is the selected option subscription universe, not every instrument in the master. |
| `CandleVolumeBridge::refresh` / `metadata` | Definitions → registered engine and pinned metadata | Registration at attach/maintenance; missing definitions do not fabricate a ready universe. |
| `MultiTfAggregator::consume_tick_with_context` | Observation and metadata → states, seals and canonical updates | All ten active frames share the quantity/classification, then fold into their own buckets. |
| `LiveCandleState::net_volume` | Classified state → signed value or `None` | Classified zero is preserved. Impossible signed magnitude is refused, not clamped into a plausible result. |
| `LiveIngest` admission path | Canonical fold plus tick-writer admission → accepted update batch | Ranking publication follows successful writer admission. Admission is not durable acknowledgment. Overflow/refusal invalidates readiness. |
| `CandleVolumeBridge::apply` / `BucketTopVolumeEngine::apply` | Supported short-frame canonical quantity, metadata, quality and revision → exact order and coverage | Filters to 1s/3s/5s/1m; no raw counter read, second differencing or database query. Handles amendments, conflicting revisions and bounded retention. |
| `publish_winners` / `publish_winner_latest` | Touched frame → bounded window of exact bucket winners for each family | Publishes after the coherent update batch. A just-closed bucket remains available when the next live bucket opens; unchanged entries retain their publication clocks. |
| `publish_tables` / `publish_latest` | Changed ordered index → immutable full board | Maintenance copies ordered rows. Timer arms trigger maintenance, not independent volume windows. |
| `BucketTopVolumeRuntime::load_winner_for_decision` | Explicit epoch, retained bucket, clock, age and closure request → winner or refusal | Examines at most eight prepared bucket headers, with declared-universe coverage and instrument freshness checks. No contract scan or full-board copy. |
| `snapshot_top_volume` depth selection | Canonical Stock/5s board → depth candidates | Existing gainer/distinct-underlying selection follows volume ordering; it does not redefine the score. |
| Candle seal writer / spill / DLQ | Canonical sealed state → history and recovery records | Carries signed quantity, pinned metadata, quality and revision. Missing legacy fields stay unknown. |
| `ensure_named_views` / `candle_top_volume_view_ddl` | Supported Top Volume `TfIndex` → corresponding SQL projection | Four primary views plus four explicitly legacy timer-snapshot views. Longer-frame historical objects are preserved but are outside active Top Volume creation and readiness checks. |
| `build_candle_top_volume_router` | Authenticated GET → bounded JSON | RAM list and strict winner reads; no database query or order submission. |

Sources: `crates/common/src/tick_types.rs`; `crates/trading/src/candles/{multi_tf_aggregator,live_candle_state,volume_update,tf_index}.rs`; `crates/app/src/{contract_underlying_map,candle_volume_bridge,bucket_top_volume,candle_top_volume_api,dhan_feed_stack}.rs`; `crates/storage/src/{shadow_persistence,shadow_seal_columns,shadow_candle_writer,console_views,candle_top_volume_views}.rs`.

The runtime supplies data for consumers and existing depth subscription steering. The new read API does not place, cancel or execute an order. Passing data-readiness checks does not establish strategy profitability or order-execution readiness.

## Candle storage and Top Volume columns

Each `candles_<tf>` table has the existing identity, time, OHLC, quantity and price-change fields plus six additive provenance fields. Its designated timestamp is `ts`; the declared deduplication key is `(ts, security_id, segment, feed)`, partitioned by day. This is an UPSERT candle history, not an append-only revision ledger.

| Physical candle columns | Purpose and source |
|---|---|
| `feed`, `segment`, `security_id`, `ts` | Feed/instrument identity and bucket-open timestamp |
| `open`, `high`, `low`, `close` | Candle owner's observed price fold |
| `volume`, `net_volume`, `tick_count` | Gross accepted delta, signed tick-rule estimate or NULL, and folded source-row count |
| `oi`, `total_buy_qty`, `total_sell_qty` | Observed quote/open-interest fields; not substitutes for executed signed net flow |
| `close_pct_from_prev_day`, `open_pct`, `change_pct`, `open_gap_pct` | Existing price-change projections; none is the Top Volume score |
| `lot_size` LONG | Positive pinned per-lot quantity, representable as u32 |
| `instrument_definition_version` LONG | Positive pinned definition version within signed storage range |
| `underlying_id` LONG | Positive pinned underlying within signed storage range |
| `ranking_family` SYMBOL | Pinned `stock` or `index`; absent when unknown |
| `volume_quality` LONG | Local quantity/metadata quality mask; zero is necessary for ranking |
| `bucket_revision` LONG | Positive canonical revision; amendments carry the newer revision |

Historical rows without the new fields remain visible and unavailable for the new score. The migration does not borrow today's lot size or fabricate a historical definition. Current definitions can supply display labels only.

All four primary views have these 27 columns in SELECT order:

| # | View column | Population and reason |
|---:|---|---|
| 1 | `ts` | Candle timestamp; selects the historical bucket opening. |
| 2 | `symbol_name` | Unique current lifecycle label or NULL; aids identification. |
| 3 | `display_name` | Unique current lifecycle label or NULL; human display text. |
| 4 | `instrument_type` | Unique current lifecycle label or NULL; display context only. |
| 5 | `family` | Candle `ranking_family`; separates stock and index comparisons. |
| 6 | `gross_volume` | Candle `volume`; total inferred activity. |
| 7 | `net_volume` | Candle signed net unchanged, including negative/zero/NULL. |
| 8 | `lot_size` | Candle's pinned denominator; never the current label join's lot size. |
| 9 | `gross_lots` | `gross_volume / lot_size` when rank-eligible, otherwise NULL. |
| 10 | `signed_net_lots` | `net_volume / lot_size` when rank-eligible, otherwise NULL. |
| 11 | `net_volume_chg_pct` | `100 × (signed_net_lots − 1)` when rank-eligible, otherwise NULL. |
| 12 | `rank_eligible` | Explicit quality, quantity, metadata and revision gate. |
| 13 | `volume_quality` | Stored mask; explains why a quantity may be unsuitable. |
| 14 | `bucket_revision` | Stored canonical revision; enables like-for-like reconciliation. |
| 15 | `instrument_definition_version` | Stored version; identifies the denominator's definition. |
| 16 | `underlying_id` | Pinned underlying, unaffected by mutable labels. |
| 17 | `feed` | Stored feed identity. |
| 18 | `segment` | Stored exchange segment; distinguishes equal security IDs. |
| 19 | `security_id` | Stored contract identity. |
| 20 | `tf` | Canonical suffix, such as `5s` or `1m`, derived from `TfIndex`. |
| 21 | `metric_version` | `signed_estimated_net_vs_one_lot_v2`. |
| 22 | `net_volume_basis` | `tick_rule_estimate`; exposes the signed estimate's basis. |
| 23 | `lot_metadata_basis` | `persisted_candle`; historical arithmetic uses its own pin. |
| 24 | `display_metadata_basis` | `unique_current_lifecycle_labels`; not an as-of historical label claim. |
| 25 | `display_metadata_rows` | Current non-dry-run lifecycle match count; NULL when absent. |
| 26 | `display_metadata_ambiguous` | True when multiple lifecycle rows match the full identity. |
| 27 | `source_table` | Actual `candles_<tf>` table behind the projection. |

The lifecycle dimension is grouped to one row per `(security_id, exchange_segment, feed)` before the LEFT JOIN. Duplicate matches produce NULL labels and explicit ambiguity/count, even for identical duplicate text. They cannot multiply a candle row or affect its score. Missing labels also retain the candle. This protection applies to the primary views and `candles_named`; it is not a claim about every legacy join.

Invalid/uncertain candles stay below eligible rows within their bucket/family, with NULL derived values. `rank_eligible` requires quality zero, non-NULL net, valid positive lot/version/underlying, known family, positive revision, nonnegative representable gross and `−gross_volume ≤ net_volume ≤ gross_volume`. Historical SQL eligibility alone does not establish whole-universe coverage or current freshness.

## Timeframes and timestamps

The common `ACTIVE_CANDLE_TIMEFRAMES` specification supplies candle labels, periods, table names and durable IDs. `TfIndex::ALL` is its typed dense runtime view and drives all ten candle frames. Configuration and candle metric labels use that same specification. The separate `TOP_VOLUME_CANDLE_TIMEFRAMES` subset and `TfIndex::TOP_VOLUME_ALL` restrict live ranking, the API and primary SQL view management to four frames.

| Scope | Supported frames | Storage and ranking relation |
|---|---|---|
| Candles and Top Volume: 4 | `1s`, `3s`, `5s`, `1m` | `candles_<tf>` → live RAM ranking and `top_volume_<tf>` |
| Candles only: 6 | `3m`, `5m`, `10m`, `15m`, `30m`, `60m` | `candles_<tf>` continues; no live Top Volume ranking or newly managed primary view |

Both option families have independent winner windows and latest full boards for each supported Top Volume frame: **8 bounded winner windows and 8 full-board slots**. A winner window follows the engine's retained buckets: two in the live bridge, with an engine maximum of eight. Its newest entry remains the latest-live accessor. Older entries are selected only by exact requested timestamp. Source support does not prove populated tables. The candle change adds 10m and retires 2s, 4s, 6s, 7s, 8s, 9s, 10s, 11s, 12s, 13s, 14s, 15s, 30s, 2m and 1d. Top Volume additionally refuses the six candle-only frames above. Existing historical database objects are not dropped.

The existing candle grid is anchored at **09:00 IST**, with the candle gate `[09:00, 15:40)`. A 09:15 observation belongs to the 10m bucket [09:10, 09:20), the 30m bucket [09:00, 09:30), and the 60m bucket [09:00, 10:00). The final 10m bucket is [15:30, 15:40). The fold uses receipt time when it is between exchange time minus 10 seconds and plus 300 seconds; otherwise it uses exchange time. Values use this repository's **IST wall-clock epoch axis**, not an unshifted UTC epoch. Clients must honor `timestamp_axis` rather than interpret the raw integer as UTC.

| Timestamp / version | Meaning |
|---|---|
| `session_day` | Day on the IST axis; separates counter/ranking sessions. |
| `bucket_start_secs` / SQL `ts` | Candle opening; no relabeling as a timer sampling end. |
| `bucket_end_secs` | Nominal start plus frame duration. |
| `observation_window_end_secs` | Exclusive end of observations admitted to this exact bucket: the earlier of nominal end and the existing regular 15:40 capture close. Invalid/out-of-window grid identities have no closing authority. |
| `closure_basis` | `regular_capture_window_v1`: the local fold's regular [09:00, 15:40) capture policy, not an exchange calendar or a special-session schedule. |
| `closure_finality` | `revisable_local_window_not_provider_finality`: later accepted data can revise a locally closed candle. |
| `last_observed_secs` | Last canonical instrument observation; independent of publication time. |
| `published_secs` | Current owner clock at publication, independent of packet receipt order. Unchanged revisions keep their original publication time; source observation clocks are never refreshed by publication. |
| Row `bucket_revision` | Instrument-candle revision carried from the owner. |
| Board `revision` | Coherent ranking publication revision; pin it with session/universe/bucket when paging. |
| `universe_version` | Declared selected-universe generation; changes invalidate old readiness. |

Under `regular_capture_window_v1`, the final 3m, 15m, 30m and 60m observation intervals are clipped at 15:40 while their nominal bucket labels and ends remain unchanged. The other six active frames, including 10m, naturally end at 15:40. Daily is no longer an active candle or API selector. The regular close controller uses an independent day/capture-window helper rather than a retired D1 enum variant.

`closed=true` means the canonical owner sealed that exact bucket after its admitted observation window expired. A natural rollover or explicit cutoff can establish that fact. An administrative flush without closing authority emits an unqualified partial state; time passing later cannot turn it into a complete-window decision. Each instrument retains one exact qualified bucket identity per timeframe so a late amendment keeps that bucket's real closure while a later bucket's closure cannot certify an older partial one. The extra bounded state is 10 × 4 bytes per allocated instrument. Late corrections still revise quantities, quality and ranking.

Strict closed reads require every selected instrument's qualified closure, both the server and publication clocks at or after `observation_window_end_secs`, and the unchanged coverage, quality and source-age checks. Both runtime strict readers explicitly require the server's current session day to match the publication; a caller cannot turn history into current data by enlarging an age budget. SQL candle history carries quantities and quality but does not itself provide the live closure authorization. Each winner window is coherent; cross-frame reads are not an atomic transaction.

The close controller uses the existing two-second catch-up margin and waits until the locally observed frame and seed queues are empty. It expires the local window without advancing instrument observation timestamps or claiming all provider messages have arrived. A subsequently consumed main-feed frame or seed batch re-arms the check on the existing one-second maintenance timer: a delayed captured frame can open a new final candle after the first sweep without moving the global watermark. Depth/order-only work does not re-arm a candle sweep. Shutdown attempts the same qualified close before its administrative flush; an undrained seed queue still refuses qualification rather than inventing universe completeness.

This is separate from the existing 240-second grace used by arrival-clock depth persistence; that admission policy remains intact. Later valid tick amendments remain possible. At the earliest close+2 publication, even an observation from close−1 is already three seconds old, so the default two-second strict age budget refuses it. An explicitly selected age up to the existing 30-second cap may permit sufficiently recent data, but quiet, missing, ambiguous or stale instruments still refuse a whole-universe decision. Timer closure alone never guarantees that a final-window winner is available.

## Active indexes and stored history

Dense runtime indexes are 0–9 in the requested order. They are not persisted timeframe identities. Storage encoders/decoders use dedicated durable-ID methods: retained IDs do not change, and new 10m appends ID 24. Historical 1d ID 4 cannot be decoded as active runtime slot 4 (3m); neither can old wire ID 0 (1m) become runtime slot 0 (1s).

Recovery preflights each staged binary/DLQ file before any sink writes. If it recognizes a retired timeframe anywhere in a readable file, it defers that entire original, including active siblings, and reports `records_retired_timeframe` and `files_deferred_retired_timeframe` with explicit boot metrics. This prevents a permanently retained, readable mixed file from repeatedly overwriting a newer active candle with an older revision. A fully decoded retired tail is known pending work, not automatically corrupt or unknown. Healthy separate active-only files remain eligible for replay after a rewind of the same handle. This preserves history without injecting retired frames into the live engine; it does not complete migration of the deferred file.

A preflight decode or framing error, unknown timeframe, missing EOF or I/O failure now also defers the whole original before sink writes, including valid active siblings. `files_deferred_invalid` and the `boot_invalid_file_deferred` metric expose this retained work, with separate incomplete-stream, undecodable-record, I/O and rewind reasons. Healthy separate files retain their existing replay behavior. The preflight uses bounded buffers but scans the file before the first flush; healthy active-only replay reads it again. Recovery time remains proportional to bytes.

QuestDB deduplication replaces an existing row with an incoming row that has the same upsert key; it is not a maximum-`bucket_revision` rule. A fully valid older file can still overwrite a newer destination row, including after a failed replay, partial acknowledgment, archive failure or later live correction. A second-pass read/decode failure discards its current unflushed batch; previously acknowledged batches cannot be undone by that discard. A terminal undecodable boundary cannot prove the contents of the unread suffix, and framing checks do not detect every semantically valid corruption. General revision-safe or exactly-once replay remains an unresolved release gate. See [QuestDB schema design](https://questdb.com/docs/schema-design-essentials/).

Closing that gate requires an exclusive recovery fence covering every destination writer, comparable revision lineage across restarts, durable input identity and destination reconciliation after a WAL-application barrier. Equal conflicting or unknown revisions must refuse recovery. Retries must reconcile against applied destination state, and startup must not admit producers merely because a recovery pass returned with pending files. This protocol is not implemented by the narrower damaged-file safeguard.

New 10m table creation refuses an incompatible pre-existing historical object rather than silently using DROP or self-healing replacement. Such a name conflict needs a separately reviewed migration. Legacy reader compatibility with the newly appended wire ID remains a release gate; source support is not a completed migration.

At the existing 400-minute regular capture span, the seven minute frames have 400, 134, 80, 40, 27, 14 and 7 nominal buckets, totaling 702. These are grid-capacity counts, not assertions that every instrument supplies a complete candle. The separate SpotBarStore retains capacity-one second rings under its existing policy; it is not all-timestamp RAM history for 1s/3s/5s. Live ranking retention remains bounded independently.

## Unknowns, failures and readiness

The quality mask describes local data, not proof that the broker delivered every exchange event.

| Mask value | Flag | Why ranking is unavailable |
|---:|---|---|
| 1 | `UNKNOWN_METADATA` | Lot, version, underlying or family unavailable/invalid. |
| 2 | `DEFINITION_CHANGED` | A supplied definition disagrees with the active candle's pin. |
| 4 | `UNKNOWN_BASELINE` | No attributable opening counter baseline was established. |
| 8 | `COUNTER_AMBIGUOUS` | Same-session counter regression or related ambiguity. |
| 16 | `MISSING_OBSERVATION` | Source packet lacks a usable volume observation. |
| 32 | `ATTRIBUTION_UNCERTAIN` | Quantity cannot be attributed to its bucket with the required local certainty. |
| 64 | `UNCLASSIFIED_NET` | Signed estimate unavailable; NULL is not silently zero. |
| 128 | `REVISION_EXHAUSTED` | A valid next revision cannot be represented. |

Flags combine by bitwise OR. Writers also refuse or flag out-of-domain quantities and metadata. A signed value can be displayed while its score is unavailable. A genuinely classified zero remains zero; an unknown first sample, missing packet or absent historical field does not become a fabricated balanced candle.

| Situation | Current response | Remaining boundary |
|---|---|---|
| First observation / mid-session attach | Seed baseline; do not allocate the day's existing cumulative total to a new candle. | Initial unobserved interval remains unknown. |
| Equal counter | Accept a genuine unchanged observation under shared quality rules. | Do not manufacture quiet instruments from silence. |
| Missing volume field | Mark missing; do not treat as reset to zero. | Packet cannot establish fresh volume. |
| Lower same-session counter | Keep accepted high-water axis and flag ambiguity; no 32-packet clean relatch. | A later larger number cannot prove the regression's cause. |
| New validated session | Reset session and rebuild declared-universe readiness. | Prior-session seals remain history, not current candidates. |
| Missing/changed lot definition | Preserve pins; refuse eligibility or invalidate the changed epoch. | Today's labels cannot repair missing historical arithmetic inputs. |
| Retained late correction | Apply the newer canonical revision and update exact order. | Not unlimited historical replay in RAM. |
| Conflicting revision / invalid quantity | Refuse or flag integrity; revoke an earlier green publication when required. | Do not substitute stale green data for accepted conflicting state. |
| Update overflow / writer admission refusal | Invalidate runtime; require a newly validated session. Same-day metadata changes and reset do not clear the fault. | No silent truncation promoted as complete. |
| Missing contracts, unknown net, stale observation | Expose diagnostic coverage; strict winner refuses. | Completeness is relative to the selected universe, not every tradable contract. |
| Database/network outage | Prepared reads issue no database query; storage has a bounded recovery path. | Admission is not durability; finite queues/disks cannot cover unlimited outages. |
| WebSocket disconnect / upstream omission | Ingestion handles detection/reconnect/recovery; freshness can stop stale decision reads. | Cannot reconstruct every missing trade's price, side and timestamp from a later cumulative counter. |

`load_winner_for_decision` requires the requested feed, family, timeframe, session, universe, metric and bucket. It checks the server's same-session day, nonempty eligible coverage for the entire declared universe, integrity state, optional qualified observation-window closure, publication age, oldest instrument age and future clocks. A fresh publication timestamp does not make stale instruments fresh.

Full boards contain eligible ordered rows plus coverage counts: expected, observed, eligible, uncertain, unavailable-net and closed contracts. SQL retains individual ineligible rows; the live list does not invent ranks for them. Diagnostic `load()` may return old/incomplete data and must not be treated as a strict decision read.

## Authenticated RAM API

| Route | Required selection | Result and boundary |
|---|---|---|
| `GET /api/top-volume` | `family=stock` or `index`, canonical `timeframe` | Diagnostic latest board. Default 50 rows, limit 1–250, offset within admitted-universe cap. Header exposes metric, units, clock axis, coverage and freshness. |
| `GET /api/top-volume/winner` | Family, timeframe, feed, session day, universe version, exact retained bucket start and metric | Strict winner for that bucket or explicit refusal. The previous closed bucket survives normal live rollover. `require_closed` still defaults to true. No SQL or order submission. |

Both use existing rotation-aware bearer authentication. Disabled authentication refuses access instead of making these routes public. The server supplies the clock; callers cannot supply `now_secs`. `max_age_secs` defaults to 2 and is bounded to 0–30; it does not waive epoch, coverage or closure checks.

The only accepted timeframe selectors are `1s`, `3s`, `5s` and `1m`. `3m`, `5m`, `10m`, `15m`, `30m`, `60m`, retired candle selectors and aliases are refused rather than silently mapped to another frame.

The list can pin `bucket_start_secs`, `session_day`, `universe_version` and `revision`; all four are required for nonzero offsets. Changed publication returns a conflict and requires pagination to restart instead of stitching different rankings together. The list serves only its latest full board. The winner route can select any exact bucket still in its bounded window. Evicted requests return a conflict; neither route silently substitutes a different timestamp or provides arbitrary history. Retained winners keep the same coverage, quality, epoch, source-age and publication-age requirements as the live winner.

Large IDs, quantities, versions, revisions and rational components are decimal strings to avoid JavaScript integer rounding. Responses are `no-store`. `data_checks_passed` describes strict data readiness only, not order authorization.

## Complexity, retention and latency

Let `N` be admitted contracts per family, `N_total` the combined admitted population, `F = 10` candle frames, `F_top = 4` Top Volume frames, `B` retained buckets and `K` output rows. The bridge currently uses **`B = 2`**; the engine accepts a bounded 1–8 setting. The standalone ranking engine caps each family at 25,000, but the production metadata builder and candle aggregator cap their combined populations at 25,000. Missing selected metadata refuses registration. Bounds constrain resources; they do not turn population-sized work into universal O(1).

| Operation | Current cost shape | Interpretation |
|---|---|---|
| Accepted counter observation | O(1) scalar work | Excludes network, lookup, allocation and scheduling. |
| Warm candle fold across frames | O(F), expected constant-time identity lookup | F is fixed today; cold insertion/catch-up is separate. |
| Exact ranking update | O(log N) per changed frame, plus expected lookup | Ordered index maintenance is not claimed O(1). |
| Strict prepared winner read | O(B), B ≤ 8; O(1) in N, fixed-size result | Bounded exact-bucket lookup; no contract scan, sort, SQL or full-board copy. No hard nanosecond bound. |
| Winner window publication | Bounded header/Arc work, up to O(B²) identity checks | Reuses unchanged entries and never copies the contract table. Arc allocation and synchronization have variable elapsed cost. |
| Changed full-board materialization and validation | Expected O(N) per board; O(F_top × N_total) for all four ranking frames | Maintained order avoids sorting again, but every row is copied and validated. This maintenance still pauses the ingestion owner. |
| Direct page response | O(K), K ≤ 250 | Direct slicing avoids offset scans; encoding/network remain variable. |
| Universe registration/reset or bucket eviction | Population-sized work | A first new-bucket tick synchronously reclaims old indexes; bucket map growth can also resize. These are real tick-path tail costs. |
| Retained ranking memory | O(N_total × F_top × B) plus current boards and held older revisions | Only four ranking slots per family. Expired corrections are refused. Readers retaining old Arcs can retain extra allocations. |
| Catch-up/final candle sweep with ranking callbacks | O(N_total × F) traversal plus O(S × log N) for S supported ranking updates | All ten candle frames still fold/seal; only four can reach ranking. Persistence adds row/byte work. |
| Historical query / output all rows | Population, sort, join and output work | Arbitrary history or millions of output rows cannot cost total O(1). |

Winner and full-board snapshots publish independently: individually coherent, but temporarily different revisions are possible. Consumers must check identity and bound retained handles. Rust does not remove output-size, network-distance, acknowledgment or scheduler costs. Constant operation count and elapsed nanoseconds are different properties.

**No new nanosecond or microsecond latency result is claimed.** Measure warm counter/fold/index work, prepared winner reads with a real clock and contention, board materialization, HTTP delivery, storage acknowledgment and feed-to-consumer delay separately. Record source revision, AWS hardware, workload, sample counts, quantiles, allocations, queue behavior and throttling. Market-open burst capacity and outage recovery need their own fault/load evidence.

## Deployment prerequisites: sequence authority and candle v3

| Prerequisite | Required behavior before rollout |
|---|---|
| Preserve the capture identity authority | Use one exclusively owned WAL directory per database/feed capture namespace. Its directory-bound `sequence.tvsq` manifest stores the durably reserved sequence ceiling; `sequence.initialized` survives normal pruning. Missing authority beside history or that marker refuses startup. Do not delete these files to force initialization. |
| Migrate an existing namespace offline | `migrate_sequence_authority` requires the held directory guard, a verified conservative sequence bound covering authoritative database/history/rescue state and retained WAL, and recorded provenance. It independently scans retained WAL and refuses corrupt/unreadable identity state, an insufficient bound or an existing authority. A `MAX` query over only currently retained database rows is not automatically sufficient. Migration preserves history and records a receipt; normal startup does not perform it. |
| Keep directory changes inside the migration plan | Moving to an empty WAL root is **not a safe identity reset**: it loses the previous durable ceiling and can reuse identities already stored elsewhere. Separate roots do not provide a shared restart-global allocator, and copying the directory-bound manifest is not a migration. |
| Preserve candle format isolation | New binary spills use `.cseal3` with 176-byte v3 records; new DLQ files use `.cseal3json`. New readers also accept legacy `.bin` v1/v2 records and `.ndjson`, preserving unknown historical metadata. Staging/archive retain the suffixes; never append or rename v3 records into a legacy suffix. |
| Verify rollback reader compatibility | Reviewed older readers discover only `.bin`/`.ndjson`, so the new suffixes keep metadata-bearing records outside their replay/archive discovery. This isolation leaves a v3 backlog for a compatible reader; it does not make an older binary able to consume that backlog or establish complete rollback success. Verify the actual rollback artifact and preserve pending files. |
| Install and validate the effective configuration | Verify both `base.toml` and `production.toml` against the intended release. The smoke binary must load the same base, environment and local override layers as the app, with the production environment selected explicitly. An incompatible local override must not bypass timeframe validation. |
| Verify artifacts before execution | Derive SHA-256 digests from the downloaded release artifacts on the runner. Compare the staged application, smoke test and migration helper with those expectations before invoking any of them or installing the release. A hash match establishes identity with the selected artifacts; it does not replace source-matched build and test evidence. |
| Verify the running release before provenance | After readiness and the separate log-ingestion check, verify a stable systemd main PID, the running executable and installed binary hashes, the helper binary hashes, the intended configuration hashes and the application's reported Git SHA. Recheck the PID and active state after file observations. No observation is a permanent guarantee against later change. |
| Keep health and identity failures distinct | A definite identity mismatch uses the existing private rollback snapshot. An unavailable probe, changing PID, or degraded or missing health status remains unverified and does not by itself request rollback or stop a possibly healthy process. Off-session degradation may require later healthy verification. Even a healthy top-level response does not prove candle/view equality, complete coverage or the opening-session load. |

**The offline sequence migration has not been executed. No live migration or deployment success is claimed; current candidate tests and latencies remain unverified.** Source: `crates/storage/src/{ws_frame_spill,seal_spill,seal_dlq,seal_writer_task}.rs`.

The Rust operator command is `tv-wal-sequence-migrate --wal-dir PATH --evidence FILE`. It requires an existing directory, obtains the same exclusive writer lock, and validates strict JSON evidence containing `highest_capture_seq`, `complete_namespace_verified: true`, and nonempty `provenance`. Without `--apply`, it validates inputs and ownership only; it does not scan retained WAL or install an authority. Once the complete namespace evidence is verified and the writer is stopped, `--apply` invokes the migration engine and prints its receipt. The command never queries a database or invents a bound. Build it from the validated source with `cargo build -p tickvault-storage --release --locked --bin tv-wal-sequence-migrate`.

## Validation status and historical evidence

Current source includes signed-ratio, zero/NULL, quality, metadata, revision, all-frame, coverage, freshness, retention, API and duplicate-label fixtures. Fixture presence is not execution evidence. Complete generated SQL must run on the pinned QuestDB version, including signed division/remainder and duplicate lifecycle rows. SQLite arithmetic/projection models are useful cross-checks, not substitutes for QuestDB or Rust execution. Isolated DDL fixtures must remain isolated from production history.

Before release, record source-matched Rust results/builds, actual ten-candle/four-primary-view migration results, pinned-engine SQL checks, canonical RAM/sealed-candle equality, invalidation/recovery, authenticated API behavior and AWS performance. Deployment additionally requires artifact identity and post-deploy service/schema checks. This document does not assert all these gates have passed.

The prepared `release-validation.yml` runs on draft pull requests or manual branch dispatch. It separates an ARM64 static build/config smoke from the existing ignored isolated candle-view SQL fixture, then requires both jobs to succeed on the same commit and tree. It has no AWS deployment action. Its SQL fixture does not exercise all ten schema migrations, and its completion status is not automatically a branch-protection requirement. Keep the pull request draft until that status, existing CI and the remaining recovery/migration gates pass. The new workflow has not executed for the current source.

**Historical only:** candidate `d54ebc68749f6ea3c097d9fe51180462df223e61` was documented with 905 selected passing tests and an individually timed fresh-handle/first-row median range of 181–189 ns. It used the previous unsigned timer-based ranking/runtime. Those counts and timings do **not** validate the current signed candle engine, changed active-frame bridge, pinned metadata, API or new views. They are not current latency estimates or a release guarantee. The earlier 760-test and rounded-key campaigns are historical too.

The old physical `top_volume` rows and four `top_volume_legacy_<tf>` views retain `gross_activity_vs_one_lot_v1` / `legacy_timer_counter_delta` provenance. Do not silently combine them with v2 rows or describe them as the same signed metric. The earlier source review in [candle-volume-assurance.md](candle-volume-assurance.md) is retained with a current preface for the same reason.
