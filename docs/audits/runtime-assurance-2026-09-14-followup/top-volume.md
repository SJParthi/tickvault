# TickVault: what the top-volume table actually means

**A decision can read the already-computed highest row from RAM without querying QuestDB. That read is independent of the number of rows. Computing the entire board is not O(1), and this repository does not yet wire that accessor into an automated trading decision/order path.**

This dictionary inspects baseline`af77fb0` plus the current candidate exact-ranking/runtime/SQL changes. It records source behavior; it does not claim the candidate Rust compiles, the SQL ran on QuestDB, the AWS service deployed it, or a live-feed stress test passed. Root owns the final validation ledger.

## Four different numbers that must not be confused

| Number | Meaning | Example for 120 new units and 20 units/lot |
| --- | --- | --- |
| `volume` | Latest accepted cumulative day counter | May be50120; not the window score |
| `delta_units` | Gross activity observed since this cadence baseline | 120 |
| Exact one-lot-baseline percentage | 100×(120/20−1) | +500% |
| Candle`net_volume` | Tick-rule signed estimate, buy-classified delta minus sell-classified delta | Can be0 even with gross120 |
| `underlying_chg_pct` | Underlying price move versus previous close | Independent, such as−2% |

The current percentage baseline is **one lot**. It does not ask whether this candle increased versus the preceding candle. For previous-window growth the formula would need previous-window volume and a defined zero-baseline policy. No code silently changes to that different requirement.

## How the data flows

1. Decode a Dhan frame and assign reproducible capture identity. The candle engine validates/gates the tick; the tick writer records accepted rows. Fully accepted non-replay option ticks resolve family/underlying/lot from the contract map.
2. The in-memory leaderboard updates a cumulative counter and marks affected cadence entries dirty. Separate 1s/3s/5s/1m baselines keep each cadence independent.
3. On each timer, consume the changed set, compute delta/lot, rank all eligible positive-volume contracts, and publish an immutable family/cadence snapshot.
4. The stock 5s ranking also supplies gainer-eligible depth20 and distinct-underlying depth200 candidates. These are subscriptions, not buy/sell orders.
5. In parallel with later processing, the offload writer persists projected snapshot rows. Querying history uses QuestDB views. A live RAM reader does not require a DB round trip.

Publication and projection still run O(R) work on the serialized feed drain. Moving the network write to another thread does not remove the ranking, copy, validation and ILP serialization cost.

[crates/app/src/dhan_feed_stack.rs](sandbox:/workspace/scratch/81296451ef3b/tickvault-audit/crates/app/src/dhan_feed_stack.rs) (`LiveIngest::ingest_tick_at / observe_for_ranking / snapshot_top_volume`); [crates/app/src/top_volume_runtime.rs](sandbox:/workspace/scratch/81296451ef3b/tickvault-audit/crates/app/src/top_volume_runtime.rs) (`TopVolumeRuntime`)

## Exactly how descending sorting works

| Surface | Primary comparison | Tie rule | Validation state |
| --- | --- | --- | --- |
| af77 baseline RAM | Integer`window_lots_milli`DESC | ID then segment | Historical baseline; fractional ties could misorder true ratios |
| Candidate RAM | Exact`delta_units/lot_size`DESC via widened cross-products/refined fixed-width key | Only true equal ratios tie; ID then segmentASC | Source changed; candidate Rust and AWS tests pending |
| Candidate QuestDB views | Newest timestamp, separate family, then valid exact ratioDESC using integer+64 fraction-bit decomposition | ID, segment ordinal/text, feed | Source changed; live DDL/query parity gate pending |
| Displayed percentage | `(floor(delta*1000/lot)*100-100000)/1000` | Display can be identical for distinct exact ratios | Still rounded to 0.1 percentage-point steps |

Synthetic precision example:14250/10000 lots gives42.50%;14251/10000 lots gives42.51%. Both stored milli-lot displays truncate to1425 and print 42.5%. The candidate ranks42.51% first from the original ratio. The table retains`delta_units` and`lot_size` so the distinction can be checked. Positive sub-milli activity is retained by the candidate even when its displayed milli-lots is0. No`rank` column is added.

Exact rational ordering is possible here because numerator and denominator have fixed u32 bounds. That does not make total sorting constant-time in row count. SQL repeats bounded arithmetic per row and still has to process/sort the selected rows.

[crates/app/src/volume_leaderboard.rs](sandbox:/workspace/scratch/81296451ef3b/tickvault-audit/crates/app/src/volume_leaderboard.rs) (`compare_ranked_contracts / exact_fractional_lots_key`); [crates/storage/src/console_views.rs](sandbox:/workspace/scratch/81296451ef3b/tickvault-audit/crates/storage/src/console_views.rs) (`top_volume_exact_ratio_order_sql`)

## Every stored top_volume column

| Column / type | Why it exists and units | How populated | Missing/edge behavior |
| --- | --- | --- | --- |
| `ts` / TIMESTAMP | Identify the published snapshot. It is not the source tick timestamp and not a candle-open timestamp. IST wall-clock convention; ILP input nanoseconds; values on whole cadence boundary | snapshot_top_volume passes now_ist_nanos(); floor_to_grid rounds down to the cadence grid anchored at 09:00 IST. Projection uses the same floor. | The label does not encode actual observed interval start/end or missed sweeps. A delayed sweep combines deltas since the previous sweep. RAM refuses nonpositive/off-grid/older values; same-boundary replacement is permitted. |
| `tf` / SYMBOL | Tell which independent cumulative baseline produced the row. 1s, 3s, 5s, 1m | SnapshotCadence::as_str; four cadence timers feed four per-contract baseline slots. | These are the only ranking cadences. The24 candle variants do not automatically create24 ranking boards. |
| `family` / SYMBOL | Keep stock and index option populations separate. stock or index | ContractOwner family is passed to VolumeLeaderboard::observe; snapshot loop publishes Index and Stock independently; OptionFamily::as_str stores label. | There is no combined all-options winner. Family is not a price/volume signal. |
| `feed` / SYMBOL | Preserve source provenance and avoid identity collisions between brokers. broker label | SNAPSHOT_FEED constant currently dhan. | A second feed is not automatically covered. Current producer supplies only Dhan. |
| `segment` / SYMBOL | Disambiguate reused security IDs across segments. exchange segment | ParsedTick exchange code→ExchangeSegment→as_str; retained with each ranked identity. | Unknown segments do not enter the owner lookup/ranking. SQL maps recognized segment names to the Rust enum ordinal for ties. |
| `contract` / SYMBOL | Show underlying, expiry, strike and CE/PE together without a live join. full human-readable option label | contract_label renders artifact metadata once at map publish; labels_from_artifact builds a composite-ID map; projection borrows the label from one snapshot. | Missing label writes unmapped and increments label_unavailable; numeric identity survives. Oversized labels are bounded and marked. Invalid expiry is rendered visibly, not invented. |
| `security_id` / LONG | Identify the exact option contract; needed for joins/subscriptions. numeric contract ID | ParsedTick.security_id retained by RankedContract; projection checked-converts u64→i64. | IDs above i64::MAX are refused as id_too_large rather than wrapped negative. |
| `underlying_id` / LONG | Connect the option to its equity or index for contextual price gain and distinct-underlying depth selection. numeric underlying ID | ContractOwner.underlying_id from artifact map; carried through RankedContract. | Missing owner means the tick does not enter this ranking. IDs above i64::MAX are refused in projection. |
| `volume` / LONG | Retain the cumulative observation used to form this snapshot for audit. provider cumulative day-volume counter; source u32 | ParsedTick.volume→VolumeLeaderboard::observe monotonicity/re-latch policy→ranked row→i64 conversion. | First positive observation seeds baselines; it is not all charged to the first window. Zero counters are ignored. Lower values follow the documented32-lower-observation re-latch policy. |
| `delta_units` / LONG | Store the numerator of the volume ratio and the activity observed since this cadence was last swept. nonnegative incremental provider volume units; source u32 | rank computes latest accepted cumulative minus this contract/cadence baseline using saturating_sub, then advances that baseline. | Zero delta is omitted. Missing first-history interval, counter reset ambiguity and merged delayed sweeps cannot be inferred away. Provider units must agree with lot_size for the normalization to mean lots. |
| `lot_size` / LONG | Normalize different contracts to comparable traded-lot ratios. units per lot; source u32 | Dhan instrument-master LOT_SIZE→artifact row.z→ContractOwner.lot_size→accepted RankedContract; rank uses carried value and only falls back to lookup when zero. | Missing/zero lot-size owner rows are refused; divisor never silently defaults to 1. Existing source does not certify live provider volume units from divisibility alone. |
| `window_lots_milli` / LONG | Provide a compact fixed-point display of the interval activity and a consistency check on delta/lot. milli-lots;1000 = 1 lot | floor(delta_units*1000/lot_size), computed with widened arithmetic. | Quantized downward to 0.001 lot. Candidate keeps a positive sub-milli delta even when this display field is0. Projection checks signed-domain conversion. |
| `net_volume_chg_milli_pct` / LONG | Expose a stored percentage display that does not require the operator to calculate it. thousandths of a percentage point | window_lots_milli*100-100000; shown as percent after dividing by1000. | Still quantized to 0.1 percentage-point steps because the input is milli-lots. Positive sub-milli activity can display -100.000%. It is not prior-candle percentage growth and not tick-rule signed net flow. |
| `gain_pct` / DOUBLE or NULL | Show whether the option underlying is up/down today, separately from traded-volume activity. percentage points of underlying price change from previous close | underlying_gain_pct reads spot_prices.latest_paise and previous close converted to paise. underlying_segment uses IDX_I for index and NSE_EQ for stock; eligible_gain_pct computes100*(spot-prev_close)/prev_close. | Missing/nonpositive/nonfinite inputs or implausible magnitude > 1000% give NULL and gain_unavailable count; volume row remains. Stores are latest RAM values, not historical as-of joins per option tick. |
| `subscribed` / BOOLEAN | Record whether a depth pool last reported holding this exact contract when projected. true/false | DepthSubscriptionView::is_subscribed probes last-published depth20 OR depth200 membership maps. | False at initial empty view. This flag is not an upstream acknowledgment, socket freshness, tick receipt or active order. Concurrent membership can change during a sweep; no row carries a membership-version timestamp. |

All 15 base columns are above. Identity is`(ts,tf,family,feed,security_id,segment)`; table partitioning is hourly. Re-emitting a matching identity UPSERTs that row when schema/Dedup is correctly active; it does not provide an atomic entire-board replacement. Historical legacy`rank` columns are not automatically removed.

[crates/storage/src/top_volume_rank_persistence.rs](sandbox:/workspace/scratch/81296451ef3b/tickvault-audit/crates/storage/src/top_volume_rank_persistence.rs) (`TopVolumeRankRow / top_volume_rank_create_ddl / TopVolumeRankWriter::write_row`); [crates/app/src/top_volume_snapshot.rs](sandbox:/workspace/scratch/81296451ef3b/tickvault-audit/crates/app/src/top_volume_snapshot.rs) (`project_snapshot`)

## Every additional or renamed view column

| Column | Source | Meaning / absence |
| --- | --- | --- |
| `symbol_name` | instrument_lifecycle.symbol_name | Current master symbol joined by contract security_id+segment+feed. NULL if no non-dry-run lifecycle row. Because lifecycle is updated in place, a later query of history can show newer metadata. |
| `display_name` | instrument_lifecycle.display_name | Current master display label; complements the persisted full contract string. Same LEFT JOIN behavior; no row is removed solely because labels are unavailable. |
| `instrument_type` | instrument_lifecycle.instrument_type | Current master type, such as option classification. NULL if no matching non-dry-run metadata. |
| `window_lots` | cast(window_lots_milli AS DOUBLE)/1000.0 | Human units for stored milli-lots; a rounded presentation value. It cannot restore the fraction discarded when milli-lots were calculated. |
| `net_volume_chg_pct` | cast(net_volume_chg_milli_pct AS DOUBLE)/1000.0 | Displayed volume percentage versus one lot. A rounded value, not the exact ratio comparator; can print the same for distinctly ordered rows. |
| `underlying_chg_pct` | top_volume.gain_pct | Unambiguous display alias for underlying PRICE percentage. NULL means unavailable, not0% and not a reason to drop the volume row. |

The four view names are`top_volume_1s`,`top_volume_3s`,`top_volume_5s`,`top_volume_1m`. Their20-column display order is:

`ts → contract → symbol_name → display_name → instrument_type → family → delta_units → lot_size → window_lots → net_volume_chg_pct → net_volume_chg_milli_pct → underlying_chg_pct → subscribed → volume → window_lots_milli → underlying_id → feed → segment → security_id → tf`

The metadata LEFT JOIN uses security ID, exchange segment and feed and excludes dry-run lifecycle rows inside the join input. It preserves unlabeled facts. Lifecycle metadata is a current-state table, so a historical view query can reflect newer names; the base`contract` column is the label persisted with the snapshot.

[crates/storage/src/console_views.rs](sandbox:/workspace/scratch/81296451ef3b/tickvault-audit/crates/storage/src/console_views.rs) (`top_volume_cadence_view_ddl / lifecycle_dim_subquery`); [crates/storage/src/instrument_lifecycle_persistence.rs](sandbox:/workspace/scratch/81296451ef3b/tickvault-audit/crates/storage/src/instrument_lifecycle_persistence.rs) (`DEDUP_KEY_INSTRUMENT_LIFECYCLE`)

## Every timeframe and its timestamp

| Timeframe | Candle computed | Candle emitted | Top-volume RAM / view |
| --- | --- | --- | --- |
| 1s | Yes | Yes | Yes / top_volume_1s |
| 2s | Yes | No | No |
| 3s | Yes | No | Yes / top_volume_3s |
| 4s | Yes | No | No |
| 5s | Yes | Yes | Yes / top_volume_5s |
| 6s | Yes | No | No |
| 7s | Yes | No | No |
| 8s | Yes | No | No |
| 9s | Yes | No | No |
| 10s | Yes | Yes | No |
| 11s | Yes | No | No |
| 12s | Yes | No | No |
| 13s | Yes | No | No |
| 14s | Yes | No | No |
| 15s | Yes | Yes | No |
| 30s | Yes | Yes | No |
| 1m | Yes | Yes | Yes / top_volume_1m |
| 2m | Yes | Yes | No |
| 3m | Yes | Yes | No |
| 5m | Yes | Yes | No |
| 15m | Yes | Yes | No |
| 30m | Yes | Yes | No |
| 60m | Yes | Yes | No |
| 1d | Yes | No | No |

**24 candle frames are calculated,12 are emitted, and only 4 cadences are ranked.** The configuration list under`engine.timeframes` is not the live candle emission selector; the real source is`TfIndex::is_operator_requested`.3s has a ranking board but its candle emission is disabled.

| Time quantity | Source and rule | Consequence |
| --- | --- | --- |
| Tick receipt | Nanosecond UTC receipt metadata; convert once into the repository IST convention for folding | Physical receipt differs from exchange trade time |
| Candle fold clock | Prefer receipt if plausible relative to exchange timestamp (−10s to+300s); else exchange fallback | Receipt-clock candle, not guaranteed trade-time candle |
| Candle`ts` | Start of grid bucket, anchor09:00 IST | 5s candle09:15:00 covers nominal[09:15:00,09:15:05) |
| Top-volume`ts` | floor(actual snapshot wall clock,cadence), anchor09:00 IST | Nominal prior 5s sweep publishes at09:15:05; not same-ts as previous candle |
| Capture sessions | Ticks/candles09:00–15:40; top-volume09:15–15:40, end exclusive | Pre-open candle data and ranking session differ |
| Timer lateness | Skip missed timer firings; cumulative baseline rolls only when sweep runs | Later delta can span multiple nominal buckets; alignment alone does not make data exact |

**A join on equal`ts` is not an exact candle/leaderboard reconciliation.** Even subtracting one cadence is only a nominal alignment while sweeps are timely; after stalls, the observed interval can be wider. A correct every-timeframe/every-timestamp design must publish from shared bucket accumulators with explicit bucket-start/end and completeness, not relabel a merged sweep as a precise candle. That follow-up is not implemented here.

[crates/trading/src/candles/tf_index.rs](sandbox:/workspace/scratch/81296451ef3b/tickvault-audit/crates/trading/src/candles/tf_index.rs) (`fold_clock_ist_secs / bucket_start / is_operator_requested`); [crates/app/src/dhan_feed_stack.rs](sandbox:/workspace/scratch/81296451ef3b/tickvault-audit/crates/app/src/dhan_feed_stack.rs) (`snapshot_timer_at / snapshot_top_volume`)

## How candle gross and net volume are calculated

The first cumulative counter seeds the instrument baseline. Each accepted advance gives a nonnegative gross delta. Price direction is classified once before the 24-frame loop: uptick→positive delta, downtick→negative delta, unchanged price→previous known direction. Real activity with no known direction marks flow unavailable; it is not falsely reported as perfectly balanced. Each timeframe maintains its own bucket/carry state.

| Observation | Price | Cumulative | Gross contribution | Tick-rule contribution |
| --- | --- | --- | --- | --- |
| First baseline | 100 | 1000 | 0; earlier volume unknown | No earlier-flow claim |
| Next | 101 | 1060 | 60 | +60 |
| Next | 100 | 1100 | 40 | −40 |
| Flat next | 100 | 1120 | 20 | −20; carries downtick |
| Bucket total |  |  | 120 | 0; actual classified balance |

With lot size20, this same gross activity is 6 lots and+500%versus one lot, while candle tick-rule net is0. The two answer different questions. Dhan snapshots can aggregate multiple underlying trades, so signing the aggregate by one observed price change is an estimate. Resting`total_buy_qty`/`total_sell_qty` do not reveal executed buy/sell volume.

| Candle field | Meaning | Population / units |
| --- | --- | --- |
| `ts` | Bucket-open timestamp, not snapshot publication timestamp. | TfIndex::bucket_start(fold_clock_ist_secs(...))*1e9 in seal row; IST convention; whole-second grid |
| `open/high/low/close` | Prices accumulated by candle engine under its day-extreme/late-amend policy. | AggregatorCell per-timeframe open/fold/seal state; Rupee prices asf64, persisted with row conversion policy |
| `volume` | Gross accepted cumulative-volume delta allocated into the bucket. | Per-instrument seeded previous cumulative plus cell bucket baseline/carry/rebase; Provider quantity units; nonnegative |
| `net_volume` | Tick-rule signed estimate: sum of classified incremental quantities, not exchange-reported aggressor flow. | classify_tick_volume once per accepted tick; per-timeframe signed accumulation; LiveCandleState::net_volume at seal; Signed provider quantity units; NULL if unclassified/no ticks/zero gross |
| `total_buy_qty / total_sell_qty` | Last nonzero resting buy/sell order quantities carried by packet. | Latest nonzero vendor fields retained in bucket; Pending book quantity; not executed buy/sell volume |
| `tick_count` | Number of folded source ticks/rows under fold policy. | Per-bucket cell counters; Source observations, not necessarily exchange trades |
| `oi` | Open interest context. | Latest usable source reading retained in bucket; Provider open-interest units |

Counter restarts and out-of-order arrivals are policy decisions: the candle engine uses a large-drop restart threshold2^31, retains late carry, and can amend a retained sealed bucket. The ranking observer uses its32-lower-observation re-latch policy. Those different policies are another reason not to claim exact cross-table equality under every fault. `net_volume()` clamps output to±gross and returns NULL when unclassified/no ticks/no gross; this presentation bound is not by itself a proof that all internal accounting is correct.

[crates/trading/src/candles/multi_tf_aggregator.rs](sandbox:/workspace/scratch/81296451ef3b/tickvault-audit/crates/trading/src/candles/multi_tf_aggregator.rs) (`classify_tick_volume / consume_tick_with_prices`); [crates/trading/src/candles/aggregator_cell.rs](sandbox:/workspace/scratch/81296451ef3b/tickvault-audit/crates/trading/src/candles/aggregator_cell.rs) (`AggregatorCell`); [crates/trading/src/candles/live_candle_state.rs](sandbox:/workspace/scratch/81296451ef3b/tickvault-audit/crates/trading/src/candles/live_candle_state.rs) (`net_volume`)

## Function dictionary and honest cost

| Function / stage | What it does | Cost | Limit |
| --- | --- | --- | --- |
| contract_label / labels_from_artifact — Cold metadata | Build stable human labels once per artifact publish. | O(metadata and output text) | Allocates on cold path; labels bounded and visibly truncated. |
| ContractUnderlyingMap::owner_of — Input identity | Map composite contract ID to family, underlying and lot size. | Expected O(1) hash lookup | Missing owner excludes ranking; expected complexity is not adversarial worst-case proof. |
| LiveIngest::ingest_tick_at — Accepted tick pipeline | Validate capture sequence, fold candle engine, persist acceptable tick, then call ranking observer only for fully accepted ticks. | O(pipeline work); candle fold O(T), T=24 | Some invalid/stale-day ticks are deliberately refused. Slot-exhausted ticks can persist while not ranking/folding. Not all received packets become ticks rows. |
| LiveIngest::observe_for_ranking — Per accepted tick | Skip replay; resolve owner; carry raw cumulative volume and metadata into family board. | Expected O(1) identity lookup plus observer | Does not rebuild historical rankings from raw WAL replay. |
| VolumeLeaderboard::observe — Per accepted tick | Insert/advance tracked volume and mark each affected cadence dirty once; unchanged volume no-op; apply lower-volume re-latch policy. | Expected O(1) for fixed 4 cadences; O(T) if cadence count scales | 25,000 contracts/family ceiling; new insertion/cold allocations and exceptional logging are not fixed nanosecond work.32-lower policy cannot distinguish all vendor resets/replays. |
| VolumeLeaderboard::roll_baselines — Outside capture hours | Consume each cadence dirty list and advance baselines without sorting. | O(D) changed contracts | Keeps pre-open accumulation out of first ranked interval; does not create missing observations. |
| window_lots_milli — Score presentation | Compute floor(delta*1000/lot) with zero-divisor refusal. | O(1) fixed-width arithmetic | Output loses sub-milli fractional detail; candidate exact comparison separately preserves order. |
| VolumeLeaderboard::rank — Cadence sweep | Consume dirty indices; compute delta and roll baseline even if ineligible; include positive valid deltas; sort full population; truncate only if caller requests k. | O(D+R) for candidate fixed-width radix/refinement, not O(1) | Actual caller requests all rows. Sweep intervals are processing intervals, not guaranteed candle buckets. |
| compare_ranked_contracts — Candidate exact comparator | Compare delta_a*lot_b versus delta_b*lot_a in u128, descending; true ties security_id then segment ordinal ascending. | O(1) per comparison at current widths | Inputs bounded positive u32; whole ranking still scales withR. |
| sort_by_exact_volume_ratio / ranking_sort::sort — Candidate whole-board sorting | Sort coarse milli-lot integer groups then refine unequal ratios inside each rounded tie by 64 fraction bits. | O(R) fixed-width keys, with fixed small-group comparison cutoffs | Exactness proof depends on denominator <= u32::MAX. Candidate Rust tests and AWS latency must validate implementation. |
| LiveIngest::snapshot_top_volume — Cadence publisher | Gate capture hours; rank each family; publish complete immutable RAM board; form stock 5s depth candidates; optionally project and append DB rows. | O(D+R) ranking plus O(R) copy/projection/serialization | Writer absence does not prevent RAM board publication. Timers may merge intervals after lag. |
| nanos_to_next_grid_boundary / floor_to_grid — Time labels | Align cadence timer and snapshot labels on09:00 IST grid. | O(1) | Scheduler delay and NTP movement are separate. A grid label is not proof of interval contents or candle parity. |
| within_capture_window — Session gate | Allow top-volume snapshots09:15:00 inclusive to15:40:00 exclusive. | O(1) | Uses wall-clock session label; does not certify full exchange coverage. |
| underlying_gain_pct / eligible_gain_pct — Price context | Compute underlying move versus previous close using current RAM stores. | Expected O(1) lookups; O(1) arithmetic | Not the volume score; missing/implausible values unavailable. |
| gainer_eligible_for_depth — Stock 5s depth selection | Preserve volume order while retaining positive underlying gainers; independently fill contract and distinct-underlying candidate lists. | O(R) scan, with expected O(1) per-underlying memo probes | Full persisted/RAM board is unfiltered. Depth20 entry 250 / exit 300; depth200 entry 5 / exit 20. This chooses market-data subscriptions, not trading orders. |
| DepthSubscriptionView::is_subscribed — Per projected row | Read depth20/depth200 last-published membership. | Expected O(1) two hash probes | Does not prove provider acknowledgment or healthy live data; maps updated concurrently. |
| TopVolumeRuntime::publish_snapshot — RAM publication | Validate grid, exact order, positive delta, valid divisor, score consistency and older-timestamp guard; replace one slot atomically. | O(R) validation | Single serialized writer contract; no cross-slot atomicity or complete-market proof. |
| TopVolumeRuntime::load — RAM read | Load one immutable family/cadence snapshot handle. | Independent ofR; constant slot lookup | Can return stale or empty board; caller must choose freshness policy. |
| TopVolumeRuntime::load_fresh — RAM read | Reject missing/empty/future/too-old board or negative age limit. | Independent ofR; constant checks after slot load | Only validates snapshot-label age, not per-instrument freshness, feed health or complete coverage. No production trading reader exists. |
| TopVolumeSnapshot::first — RAM decision input | Return first already-sorted row without DB/search/sort. | O(1) inR | Reading a winner does not include computing it, risk checks, network transit or order execution. |
| TopVolumeRuntime::reset_daily — Daily reset | Clear eight latest slots while old held Arcs remain valid immutable objects. | Fixed 8 slot operations; O(T) if cadence count grows | No atomic reset of all slots together; serialized owner required. |
| project_snapshot — Persistence projection | Apply common grid label, checked ID/score conversion, metadata and subscription context. | O(R), expected O(1) lookups/row | Missing gain/label preserves row; invalid signed IDs or arithmetic range can drop it. Per-sweep buffers allocate. |
| TopVolumeRankWriter::append_row — Producer serialization | Write one ILP row transactionally relative to buffer using marker/rewind. | O(serialized row bytes) | Bad row counts/refuses; no whole-snapshot transaction. |
| TopVolumeRankWriter::flush / split_for_offload — Storage handoff | Hand bounded batch to writer thread; full queue retains pending rows for retry. | Depends on batch bytes and handoff | Width cap/sink gone can discard snapshots. No snapshot spill tier and no zero-loss history guarantee. |
| TopVolumeRankWriterSink::write — Offload storage | Perform ILP HTTP batch write/ack outside feed-drain network path. | O(batch bytes)+I/O | Database failure can lose periodic snapshot history while RAM selection continues. |
| ensure_top_volume_rank_table — Boot schema | Rename known legacy table, create/alter missing columns and enable composite UPSERT key. | Fixed schema statements plus remote I/O | Fail-soft retries can leave DEDUP unavailable; old rank column is not automatically dropped. |
| top_volume_cadence_view_ddl — Analyst view | Create per-cadence SQL projection with LEFT JOIN labels and deterministic newest/group/exact-volume ordering. | Query work/memory grow with selected population | No automatic ordering guarantee for arbitrary SELECT from base table. SQL engine execution remains a release gate. |
| top_volume_exact_ratio_order_sql — Candidate SQL exact order | Generate valid-domain flag, integer ratio part and64 fraction bits in signed-safe31 + 31 + 2 limbs, descending. | O(1) arithmetic expressions/row, then SQL sort over selected rows | Exact only positive u32 numerator/denominator; invalid legacy rows remain visible after valid rows in same snapshot/family. No new rank column. |
| global_top_volume_runtime — Shared store access | Return process-wide OnceLock-initialized eight-slot RAM runtime. | Constant slot-store handle access after init | Source search found publication/reset and tests only: no production API/frontend/order-strategy consumer. |

`D`is dirty/changed contracts,`R`is emitted ranking rows,`T`is configured timeframes. OutputtingR rows has at least linear output work. A fixed-width field comparator can be O(1) while a full sort overR rows is O(R). A fixed number of timeframes is a bounded factor; dynamically increasing that number increases work. No nanosecond figure is claimed for this unvalidated candidate.

## Failure and extreme-case comparison

| Scenario | Actual response / remaining need | Evidence status |
| --- | --- | --- |
| Start mid-session / first positive tick | Baseline seeds to first known cumulative. Earlier activity is unknown; it is not invented as this window volume. | implemented policy; incomplete first interval |
| Same percentage display, different exact fractions | Candidate compares delta/lot exactly; higher true ratio wins even if both display 42.5%. | candidate fix; Rust/AWS validation pending |
| Positive amount below 0.001 lot | Candidate retains row even if display milli-lots=0 and displayed percentage=-100%. | candidate fix; display remains approximate |
| Exactly equal true ratios | security_id then segment enum ordinal ascending only stabilize ties. | candidate comparator; no other market signal changes primary order |
| Nothing changed in interval | RAM publishes explicit empty board; load_fresh refuses it. DB receives no rows for that empty board. | implemented; DB absence alone cannot distinguish quiet from failed write |
| Missing lot size or contract owner | Refuse ranking input, count relevant metadata/owner refusals where supported. | implemented; cannot claim full universe |
| Missing underlying price/previous close | Keep volume row with NULL contextual gain; stock depth candidate is Unknown and excluded. | implemented |
| Duplicate/equal cumulative packet | Observer treats equal as unchanged; candle incremental delta=0. | implemented; ticks may still record received snapshots with capture identity |
| Small cumulative drop | Ranking refuses lower values until 32 lower observations trigger re-latch; candle path uses different large-drop restart heuristic. | known semantic gap; no universal reset inference |
| Counter dip followed by staircase recovery | Ranking source records a residual phantom-delta possibility during staged recovery. | unresolved policy ambiguity |
| Large counter restart with pending late carry | Phase2 rebase preserves counted pending carry before rebasing open/sealed cell state. Reset tick delta itself is unknown. | candidate fix; new Rust cases unexecuted in this dictionary |
| Delayed/reordered receipt | Candles prefer bounded-plausible receipt clock; late policy can amend retained bucket or account a discard/carry. | implemented policy; not perfect exchange-time attribution |
| Timer stalls several intervals | Next rank measures cumulative difference since last actual sweep and labels current grid. Missing individual prior ranks cannot be recovered from latest counter. | known unmet exact-timeframe requirement |
| Rank timestamp versus candle timestamp | Rank ts labels publication boundary; candle ts labels bucket open. Nominal prior 5s rank at09:15:05 relates to candle09:15:00, not same-ts candle. | source-confirmed semantic mismatch |
| Wall-clock moves backward/forward | RAM refuses older/future-at-read snapshot; timer reanchor reduces drift. Existing row data still not a formal complete-window proof. | implemented guards; timing correctness not absolute |
| All stocks falling or gain unavailable | Full volume board still records volume; stock-depth gainer candidate list can be empty and existing held sockets can remain. | implemented selection policy |
| One underlying owns hundreds of highest rows | Depth200 distinct-underlying selection scans full eligible ranked population independently of depth20 cut. | phase2 candidate fix; prevents shared-cap truncation |
| Reader wants two timeframes atomically | Eight slots are independently published; each immutable board coherent, multiple loads can be from different sweeps. | known limit; no cross-cadence transaction |
| A fresh board includes stale individual input | load_fresh checks board timestamp only; it does not prove instrument latest tick age or complete feed state. | known missing decision-quality gate |
| Database slows or fails | Ranking and latest RAM board do not query DB. Bounded offload snapshots can lose history on width cap/sink failure. | implemented isolation; no lossless snapshot audit claim |
| Raw frame queue/disk/provider unavailable | Finite queues and disk cannot retain unbounded traffic/outage. Replay protects only surviving captured data; provider gaps may not be replayable. | fundamental and operational limits |
| Universe exceeds 25,000 | Ranking has25,000 slots per family; candle engine25,000 total instrument slots. Some ticks can persist even when candles/ranking excluded by slot exhaustion. | source-confirmed capacity limit |
| All 24 timeframes requested for top-volume | Current four ranking baseline slots and four SQL views do not cover 20 other candle variants. | not implemented; needs common bucketed producer and bounded capacity design |
| Millions of viewers | No top-volume HTTP/WebSocket API/frontend consumer found. UI distribution load was not tested. | not implemented/validated |
| Every source trade requested with never-disconnect promise | Dhan API snapshots are not a guaranteed exchange trade tape. Network/provider outages cannot be prevented universally. | unsupported guarantee |
| Database query without explicit ORDER BY | Base top_volume has no inherent physical percentage sort; use verified view/explicit bounded query. | query contract |
| Historical metadata renamed later | View uses current lifecycle labels; base contract string remains written label. | source-confirmed temporal metadata behavior |
| Candidate exact-SQL data outside positive u32 domain | Rows remain visible after valid domain rows within own timestamp/family; safe substitute divisor prevents zero division. | candidate policy; live SQL execution pending |

## Dhan diagram compared with TickVault

| Dhan diagram principle | TickVault source match | What it does not prove |
| --- | --- | --- |
| Database outside hot path | RAM rankings and asynchronous network writer | Ranking/projection still cost work; periodic snapshot history can be lost under pressure |
| Canonical ordered event stream | Captured-frame identity, accepted tick fold, per-instrument state | RetailAPI does not become a full exchange trade tape or provide all missing events |
| Live snapshot cache | Eight immutable latest top-volume boards | No built-in signal execution, HTTP/WebSocket API or millions-of-viewers test |
| Backpressure-aware queues | Finite frame/seal/writer queues and storage rescue paths | Cannot absorb an unbounded outage; latest-state catchup is not every-tick retention |
| Redundant feed / co-location / leased lines | TickVault consumes documented retail DhanAPI | No evidence TickVault owns or can select Dhan internal exchange fast lane |
| Rapid failover / low latency | Reconnect/liveness guards and retry policies exist | Never-disconnect and every-event/no-loss guarantees are not supplied by a diagram |

[Dhan architecture article](https://madefortrade.in/t/from-the-exchange-to-your-screen-how-dhan-built-a-fast-market-data-platform-for-retail-traders/92131) and [official live-market-feed documentation](https://dhanhq.co/docs/v2/live-market-feed/) describe different layers: provider infrastructure and the client interface TickVault can consume. No direct upstream SLA or replay guarantee is inferred from the architecture graphic.

## What is needed for the complete requested outcome

| Proposed follow-up | Why | Status |
| --- | --- | --- |
| Build rankings from shared per-instrument bucket accumulators keyed by family,timeframe,bucket_start and session. | Makes score interval and candle interval explicitly comparable; existing wall-clock sweep deltas do not. | design proposal, not implemented here |
| Publish immutable board with bucket_start,bucket_end,published_at,feed/coverage status and configuration version. | A timestamp alone cannot distinguish full bucket, partial start, late correction, delayed merged sweep or stale instrument. | design proposal, not existing fields |
| Add a RAM-only signal reader that refuses unready/stale/incomplete snapshots and emits observable decision intents; integrate actual order execution only through established risk/idempotency controls. | The current first-row accessor is not an automated trading strategy. | design proposal; no orders placed or wiring claimed |
| Record a snapshot manifest with expected/appended/acknowledged row counts and completeness. | No rows currently means either quiet board or failed snapshot persistence; base row table cannot distinguish. | design proposal |
| Validate original Dhan volume units, replay limitations and every-packet coverage against raw capture and independent provider contract. | A cumulative counter can recover quantity across a gap but not each lost trade price/timestamp/side. | verification requirement |

A complete release requires compiling and testing the actual Rust candidate on AWS, exercising generated SQL on the deployed QuestDB version with populated precision-edge fixtures, reconciling accepted input/durable replay/candle totals, and measuring processing plus publication tails under bounded specified load. Tests can establish an operating envelope; they cannot prove every future network/provider/disk failure impossible.
