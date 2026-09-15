# Signed candle volume: September 15, 2026

This describes the local v3 source change and the AWS evidence that motivated it. It is **not a deployment certificate**. The observed AWS runtime was `7f97c32c310807a2e4a3307b4b10b8463ad5d05d` at 05:41 UTC and remained that release in the 08:27 UTC health observation. Validation is recorded per exact candidate below; later source cannot inherit an earlier pass. Linux ARM64 release build/config checks, the production sequence-authority migration, physical column migration, deployment and target-instance performance measurements remain pending.

## What differs

The existing candle `volume` is incremental traded quantity inferred from accepted cumulative-volume snapshots. The deployed `net_volume` independently signs individual increments using tick-price direction and sums them. Opposing increments can cancel. A bar with 39,600 units can therefore have stored net volume of 18,000 without a subtraction or overflow bug.

The six supplied Dhan chart screenshots are for TECHM 29 SEP 1600 CALL, `security_id=39582`, `NSE_FNO`, 5 seconds. Their signs support signing a whole bar using its close against the previous chart-bar close. This is an inference from these observations; the screenshots and public Net Volume help page do not establish Dhan's complete study implementation, equality rule or first-bar fallback.

## Exact observed comparison

All row labels below are September 15, 2026, IST. AWS values were read using the full key `feed='dhan', segment='NSE_FNO', security_id=39582`.

| 5s bar | Chart Net Volume | AWS gross volume | AWS stored tick-rule net | Signed AWS whole bar, SQL preview |
|---|---:|---:|---:|---:|
| 09:15:00 | +600 | 16,800 | +12,000 | NULL: predecessor unavailable |
| 09:15:05 | +46,200 | 29,400 | -27,000 | -29,400 |
| 09:15:10 | +18,600 | 18,000 | +7,200 | +18,000 |
| 09:15:15 | +27,600 | 28,200 | +13,800 | +28,200 |
| 09:15:20 | -30,600 | 30,600 | -3,000 | -30,600 |
| 09:15:25 | +39,600 | 39,600 | +18,000 | +39,600 |

The last column was returned by a read-only AWS SQL calculation. It is a preview over the stored AWS candles, **not execution of the changed Rust binary**. Only the last two samples match the chart after this projection alone.

At 09:15:05 the chart candle is red (`open=31.60`, `close=30.95`) but its predecessor's close is 20.65, so close-to-close direction is positive. Signing by candle color would disagree with this screenshot. AWS instead has previous close 32.95 and a different first-bar distribution, so projecting AWS's own bars correctly remains negative there.

## Boundary and opening policy changes in the latest source

One captured 600-unit increment has Dhan timestamp `09:15:14`, receipt `09:15:15.112105`, price 32.60 and cumulative volume 65,400 after 64,800. The observed deployed binary chooses receipt time when it is within -10 to +300 seconds of exchange time. That assigns the increment to the later candle. Grouping these captured ticks by exchange timestamp instead reproduces chart magnitudes 18,600 and 27,600 at 09:15:10 and 09:15:15.

The latest local source separates two clocks. Validated Dhan Last Trade Time (LTT) determines candle placement; the existing bounded receipt helper determines observation freshness and falls back to LTT when receipt evidence is missing or outside its -10 to +300 second band. TrueData retains its existing receipt-based bucket policy. Session, epoch, receipt-day and future-skew checks remain in front of the fold. A regressing Dhan LTT cannot replace the last accepted current price or a later-event candle close; eligible late amendments retain their attribution uncertainty. This is a source change, not a production clock switch or a historical correction already applied.

The first captured tick at `09:15:00.067930` has cumulative volume 600 and last-trade quantity 600. The deployed counter initializes from that snapshot, and the stored first 1s candle has volume 0. Across the six samples the absolute chart readings total 163,200 versus AWS gross 162,600. The 600-unit warmup omission explains this total difference, but not the opening-bar distribution.

The latest source admits a narrow opening exception only when all of these conditions hold: it is the first present counter in the validated session, the feed is Dhan on NSE/BSE equity or F&O, LTT is exactly the 09:15:00 opening second, a positive receipt floors to that same IST second, and positive cumulative volume equals the packet's positive last-trade quantity. That observed opening quantity receives a zero origin and is counted once. A prior price-only packet does not use up this exception; an already-seen counter does, including after a same-session reset. Ordinary restarts, later arrivals, absent quantities, differing cumulative totals and other segments remain baseline-only. No general zero baseline is invented, and no historical backfill was applied.

This makes the attribution rule explicit for received snapshots. It still cannot reconstruct the prices or exact event times of unseen trades inside a cumulative jump, prove complete exchange delivery, or establish Dhan's historical-chart opening rule.

The current source additionally tracks the last accepted *present* cumulative-counter observation, including an unchanged counter. A missing counter cannot shorten this interval. If a positive jump spans a timeframe boundary and is not fully accounted for by a coherent positive last-trade quantity, the affected outgoing/incoming or retained candle is marked attribution-uncertain. A counter regression cannot certify a bucket either. The canonical gross increment is preserved; the code does not invent timestamps to split it. A diagnostic signed cache may still be displayed, but strict Top Volume ranking refuses the quality flag.

For example, the captured counter rises from 17,400 at LTT 09:15:04 to 21,000 at LTT 09:15:05. Its 3,600-unit delta exceeds the reported 600-unit last trade. The other 3,000 units have no individual execution timestamps in that snapshot. The 1s and 5s boundaries therefore cannot receive a claim of precise attribution from this packet alone. A larger timeframe that contains the entire interval does not inherit uncertainty solely because the smaller boundary was crossed. The new regression suite also covers missing counters, timer-sealed retained candles and removal of previously published eligible winners.

## Offline reconstruction using the new clock and opening policy

The file `evidence/volume-39582-20260915/ltt-opening-policy-reconstruction.json` reconstructs the captured AWS tick export from 05:41:16 UTC. It examined 63 ticks, with no event-time or counter regressions in that sample. This is an **offline calculation over captured data**, not execution of the changed Rust binary or a replacement for the observed AWS rows above.

| 5s bar IST | Reconstructed gross | Reconstructed close | Previous 5s close | Reconstructed signed volume | Chart Net Volume | Signed match |
|---|---:|---:|---:|---:|---:|---|
| 09:15:00 | 17,400 | 32.95 | Unavailable | NULL | +600 | No |
| 09:15:05 | 29,400 | 30.95 | 32.95 | -29,400 | +46,200 | No |
| 09:15:10 | 18,600 | 32.60 | 30.95 | +18,600 | +18,600 | Yes |
| 09:15:15 | 27,600 | 33.35 | 32.60 | +27,600 | +27,600 | Yes |
| 09:15:20 | 30,600 | 32.40 | 33.35 | -30,600 | -30,600 | Yes |
| 09:15:25 | 39,600 | 34.50 | 32.40 | +39,600 | +39,600 | Yes |

The first 30 seconds now total 163,200 gross units, matching the sum of the six chart magnitudes. Four signed bars match. **The first two bars still differ:** captured events give 17,400 then 29,400, while the chart gives 600 then 46,200. The captured second bar closes below its own predecessor, so its sign remains negative. The first bar has no previous same-timeframe close and remains NULL. Matching the total does not justify redistributing these events or inventing a predecessor to force chart parity.

A separate offline sensitivity experiment exactly reproduces the first two chart bars if it keeps the first 600-unit trade, omits eight intermediate captured packets, and resumes at the packet received at 09:15:06.096892. That produces the chart's opening distribution and second-bar OHLC. **This is a hypothesis about differing consumer observations, not proof that Dhan omitted those packets.** Production capture must not discard those eight packets to imitate the chart. The experiment is preserved in `evidence/volume-39582-20260915/opening-sparse-consumer-hypothesis.json`.

The user's tick query begins at 09:15:59, after all six screenshot bars. Compare bounded, matching intervals, full instrument identity and deterministic ordering. Stored `Z` labels here use the repository's IST wall-clock epoch convention; treating them as ordinary UTC would introduce a 5h30m error.

## Internal gross conservation verified on AWS

For `[09:15:00, 09:16:00)`:

| Source | Candle rows | Sum of gross volume | Sum of old net volume | Sum of tick count |
|---|---:|---:|---:|---:|
| candles_1s | 60 | 387,000 | 109,800 | 118 |
| candles_5s | 12 | 387,000 | 109,800 | 118 |
| candles_1m | 1 | 387,000 | 109,800 | 118 |

These totals prove agreement for the captured rows in this minute, not completeness of the exchange trade stream. The cumulative counter reaches 387,600; the warm baseline is 600.

## Current v3 source contract

Keep one canonical nonnegative magnitude `G` for candle folding. Cache its signed projection `S`:

| Condition | Signed `volume` S |
|---|---:|
| Close greater than frozen previous same-timeframe close | +G |
| Close lower than frozen previous same-timeframe close | -G |
| Equal consecutive closes | 0 |
| No valid prior same-timeframe close, ambiguous counter, no folded observations, legacy basis or unrepresentable signed magnitude | Unavailable / NULL; no guessed direction |

Other quality flags remain attached and prevent strict ranking eligibility. A known display projection does not certify complete attribution or fresh data. The previous-close baseline is pinned when the candle opens; a late amendment revises its own candle but does not silently rewrite a successor's pinned baseline. The first bar of a new session does not reuse yesterday's direction. These are explicit local semantics, not a promise of exact final chart history.

`G` must remain nonnegative internally: trading activity cannot be subtracted merely because price fell. Signed bar values are **not additive across timeframes**. Each of the ten timeframes folds the shared gross increment into its own bucket and signs the result against its own baseline. Summing signed 1s values cannot produce the signed 1m value in general.

| Surface | New behavior |
|---|---|
| Live candle owner | Refreshes whole-bar cache in fixed work after open/fold/amend/carry/annotation; no tick-rule accumulation. |
| Dhan clock and opening | Validated LTT places the candle; bounded receipt evidence provides freshness. Only the narrowly qualified opening trade can establish a zero counter origin. |
| Candle publication | Carries the owner's gross magnitude, optional signed cache, pinned lot size, quality, bucket and revision. |
| Top Volume engine | Uses `signed_bar_volume_vs_one_lot_v3`; rejects a claimed nonzero signed value whose magnitude differs from gross. |
| Top Volume v3 API | One signed `volume`, `signed_lots`, `volume_chg_pct`; no second v3 `net_volume` quantity. Explicit legacy diagnostic formats remain versioned. |
| Primary SQL Top Volume views | One signed `volume`, `volume_lots`, `volume_chg_pct`; legacy basis is unavailable and ineligible. |
| Physical candle storage | Gross `volume` and compatibility `net_volume` cache remain. New `volume_basis` identifies v3; a physical column drop is not part of this patch. |

Candles: **1s, 3s, 5s, 1m, 3m, 5m, 10m, 15m, 30m, 60m**. Top Volume: **1s, 3s, 5s, 1m**. This describes the candidate, not a claim these changes are already on AWS.

## Lots, ranking and percentage

Let `L` be the valid positive lot size pinned from the instrument definition when the candle opened. Signed lots are `S/L`. The existing one-lot-relative score is retained as `100 × (S/L - 1)`. It is **not growth over the previous candle's volume** and is not percentage price change.

For an example definition with `L=600`:

| S | Signed lots | Score |
|---:|---:|---:|
| +39,600 | +66 | +6,500% |
| +600 | +1 | 0% |
| 0 | 0 | -100% |
| -30,600 | -51 | -5,200% |

The old deployed candle rows do not pin lot metadata, so this example is not proof of those historical rows' definition version. New rows preserve that version. Exact signed ratios are sorted descending within the same family/timeframe/bucket; no absolute value or rounded display percentage determines order. Stable instrument identity breaks exact ties.

## Storage compatibility and rollout

The candidate schema has 25 columns: the original 18, six pinned metadata/quality/revision columns from the previous candidate, and nullable `volume_basis SYMBOL`. Fresh whole-bar caches use `signed_bar_volume_vs_one_lot_v3`. Legacy tick-rule or unknown caches use explicit legacy provenance and cannot publish as current volume.

Binary spill format 4 remains 176 bytes and writes `.cseal4`; new DLQ files use `.cseal4json`. Recovery recognizes older namespaces, marks their signed caches with `VOLUME_QUALITY_LEGACY_BASIS` (512), and preserves their raw content rather than relabeling it. Old physical columns remain to support that recovery path. Rollback cannot assume an older binary understands newly written files.

The startup source propagates an exhausted candle readiness probe or schema ensure as an error before the seal writer starts. It also requires all four primary Top Volume views to converge: HTTP success must contain `ddl: "OK"` without a SQL error. Each primary view is attempted even if another fails. A failed primary projection blocks successful candle startup; optional analyst and legacy views remain best effort. Readiness tests are part of the recorded Mac campaign; the broader release still requires the target-specific and operational gates.

The latest boot recovery source no longer blindly rewrites every staged candle. In batches of at most 64, it waits for the selected tables' WAL to apply, reads the composite keys and all 25 persisted fields, inserts missing rows, and requires an applied readback before confirming them. Identical rows are recognized without another write. Any differing known payload, including a differing revision, remains a conflict; the original staged file remains available for reconciliation. `bucket_revision` is local to an instrument instance and supplies **no global order across process restarts**. A larger replay revision therefore does not authorize an overwrite.

One narrow legacy duplicate rule preserves old unknown provenance: when every original 18-column value agrees, all seven added database fields are NULL, and the decoded replay also has no known metadata/revision plus only its decoder-synthesized legacy quality flags, recovery may recognize the row without writing. It leaves those NULLs untouched. A known value on either side must satisfy normal full-field comparison; a NULL basis alone cannot authorize relabeling an old row as v3.

Recovery is one boot pass under exclusion of the application's managed candle producers. Starting any live writer cycle consumes that recovery opportunity. SQL reads and ILP writes are not an atomic compare-and-insert transaction: independent external writers are outside this exclusion and are unsupported during recovery. Retained conflicts remain unresolved data, not proof of complete recovery. The isolated guarded-recovery fixture passed on the specifically identified Mac/QuestDB candidate below; it does not certify newer source or the Linux deployment environment.

Release must validate the exact source with Rust and the intended QuestDB version, verify migration failure behavior before writers start, test old/new spill and DLQ recovery, and then check deployed commit/schema/read surfaces. The current production instance was not restarted, its schema was not changed, and no rows were rewritten during this audit.

## Complexity and evidence boundaries

| Operation | Honest complexity / evidence |
|---|---|
| Project one already-folded candle into signed volume | O(1), fixed comparisons and checked conversion; no allocation or DB query in the projection. |
| Fold the configured ten frames | Fixed bounded frame loop; O(F) if frame count is treated as variable. |
| Maintain the current exact ordered index | O(log N) per affected ranking row. |
| Read one prepublished winner under bounded metadata checks | Fixed bounded work; requires previously maintained index and valid readiness. |
| Return K rows | At least O(K) output work. |
| Historical scans, replay and migrations | Proportional to selected records/bytes; not O(1). |
| Nanoseconds, microseconds, end-to-end latency | Unmeasured for this candidate; no numerical latency claim. |

Added and migrated Rust tests cover positive/negative/tie projections, unavailable first bars, session resets, overflow, prior-bar amendments, gross conservation, ranking/API propagation, LTT/receipt boundary separation, the qualified opening trade, regressing event times, projection-readiness failures, and guarded legacy replay. The release workflow requires exact test selectors for both the candle-view and guarded-recovery fixtures on isolated QuestDB, alongside Linux ARM64 build/config validation. The Mac SQL results below are actual executions, while the required Linux target validation remains separate. A finite campaign cannot guarantee every failure permutation or zero upstream data loss.

## Recorded validation snapshot: candidate 2ea8574

All results in this section belong to source `2ea857464bd8550b00ab1e25382a5d9a9f7ed7e8`, tree `bbf5dcd79223779e5f9322af6d2db014d7005969`, on `aarch64-apple-darwin`. These first-round results are retained as evidence of what ran; fixes made afterward cannot inherit a pass from this source.

| Rust suite | Passed | Failed | Ignored |
|---|---:|---:|---:|
| common library | 1,059 | 0 | 0 |
| trading library | 1,804 | 3 | 2 |
| storage library | 1,425 | 1 | 2 |
| app library | 2,408 | 3 | 6 |
| trading clock guard | 4 | 1 | 0 |
| storage recovery integration | 10 | 0 | 0 |
| app startup guard | 10 | 1 | 0 |

Formatting passed. The failing cases are being resolved; there is no all-tests-passed verdict for this campaign. Ignored tests are not counted as executions in these library totals.

Both explicitly selected SQL fixtures subsequently ran with `--exact --ignored` against an isolated native ARM64 Mac QuestDB **9.3.5**, using **Corretto 23**. Each returned exit code 0 and exactly one passing test:

| Exact SQL fixture | Result |
|---|---|
| `console_views::candle_top_volume_views::tests::candle_top_volume_questdb_duplicate_labels_preserve_one_row_per_candle` | 1 passed |
| `seal_recovery_guard::tests::isolated_questdb_recovery_inserts_skips_and_refuses_conflicts` | 1 passed |

Evidence: `evidence/volume-release-20260915/questdb-sql-round1.json` records the source/tree and storage test binary SHA-256 `daeb830d2f234cb9417387c855ddd9b4f4065d08907f7ddea874bf6a466dae5d`. The Rust results are in `rust-round1-results.json` and its progress snapshot. The report generator prefers `rust-final-results.json` and `questdb-sql-final.json` when later evidence exists; it never converts missing evidence into a pass.

These SQL passes establish the two fixture behaviors on that engine and candidate. They are not a production migration, Linux ARM64 musl build/config pass, AWS deployment, latency measurement, or proof that every possible replay/concurrency failure was exercised.

## Later recorded validation and operational blockers

Candidate `2ea178c402ed3732d91e29cf91c08fd05ec8297f`, tree `159d53c46798fb03b4a370e0a1a5e1e1262e89f0`, subsequently passed 9,669 ordinary Rust tests, formatting and Clippy on the private native ARM64 Mac. Eleven ordinary-run tests were ignored; the two SQL fixtures were separately selected and passed on that same source. The first-round failures above are historical results, not unresolved failures in that candidate.

Candidate `3e21d93a290c072d2698cf7e209c8d83ddb1ea36`, tree `79992c93cddd9ff66eebd8a5d07664e96c7d94cf`, adds the boundary-attribution guard and namespace/release preflights. Its recorded run passed all 1,817 trading tests, 1,437 storage tests, ten focused attribution tests, ten inspector tests and four migration-CLI tests. The focused counts overlap their library suites and must not be added as unique tests. Four tests were ignored across these two library suites. That run also found one failing release-gate test and one Clippy finding; subsequent fixes require their own execution record. See `preflight-run1-results.json`.

Read-only AWS inspection confirmed live WAL history without either durable sequence-authority file. Additional preserved recovery/spill tiers exist outside the initial root/archive inventory. Migration requires a verified conservative sequence bound over the complete identity namespace, exclusive stopped writers and a persistent compatibility fence. An old binary unaware of the new authority is not a safe rollback after migration. The read-only inspector does not manufacture that evidence or apply a migration. The separate operator documents specify the unfinished data-preserving cutover requirements.

The private Linux ARM64/Musl toolchain successfully compiled and executed a tiny probe. Automatic approval review then rejected Cargo contacting public package registries using the private source's dependency metadata. No candidate Linux release build completed. Automatic approval review separately rejected public source/configuration publication while release blockers remained. Neither rejection was bypassed, and neither toolchain readiness nor private test success is reported as a merge or deployment.

## External reference limits

[TradingView's Net Volume help](https://www.tradingview.com/support/solutions/43000589192-net-volume/) describes uptick/downtick volume but supplies no complete executable formula or initialization rule. [TradingView's Volume Delta help](https://www.tradingview.com/support/solutions/43000725057-volume-delta/) describes a different intrabar estimation algorithm. Neither establishes that the retail feed contains every exchange trade or actual trade-aggressor identities. The AWS comparison above is independent evidence and its clock and warmup limits remain material.
