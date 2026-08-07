# TF-diet lane — rescue notes (preservation snapshot, 2026-07-21)

> Purpose: one-file handover so the TF-diet lane context survives loss of the build
> container. The lane is PAUSED pre-respec; this push preserves gated WIP only.

## Lane state

- Branch `claude/tf-diet-11-broker-1d`; draft PR #1696 is the vehicle — do NOT merge as-is.
- Preserved Commit A: TF_COUNT 21 -> 12 (retire 6m..14m live aggregation). Gates green at
  the preserved tree: cargo test 1001 + 711 + 1448 passed / 0 failed; `cargo fmt --check`
  clean; `cargo clippy --workspace --no-deps -D warnings` clean.
- Commit B (retire 30m, 12 -> 11) NOT started.

## Pending operator frame-set revision (the reason the lane is paused)

- Operator revision PENDING, verbatim: "1 second till 15 second sequentially one by one
  and 30 second and 1m, 3m, 5m, 15m alone".
- Clarification (replace-vs-add; second-scale feasibility; the 1d question) is owned by
  the coordinator. Do NOT resume TF cuts until the clarified contract lands.
- Second-scale frames would be a materially deeper change than the minute-set diet:
  TfIndex ordinal/bucket math, the seal/spill format, and the QuestDB candle DDL all
  assume minute-scale today.

## Invariants and laws (stand regardless of the final frame set)

- Retired candle tables are write-stop ONLY — never added to `RETIRED_QUESTDB_TABLES`
  or any boot-DROP union; stored history is kept.
- 1d law (11-set design): never live-aggregated; broker-pulled on the next trading day
  pre-market (M3b; docs-first REST grants = M3a under the no-rest-except-live-feed lock).

## Pitfalls (hard-won; re-read before resuming)

- `crates/storage/tests/partition_retention_coverage_guard.rs` pins the candle-table
  count as a BARE literal (doc line ~102 + assert ~119) — invisible to the `; 21]`
  sweep; it must be included in ANY frame-count change (fixed in the preserved commit).
- String-only "21 TF" prose remains in: rest_candle_fold.rs, metrics_catalog.rs,
  subsystem_memory.rs, lib.rs, main.rs, candle_ddl_boot.rs, d2_stage2_hoist_guard.rs,
  ensure_ddl_boot_wiring_guard.rs. The main.rs "all 21 timeframes" string is the M4
  ratchet target.
- market_hours parallel-test flake: a process-global test-hours override race — passes
  solo and in full runs.
- Build ops: thin-provisioned rootfs can ENOSPC — delete stale
  `target/debug/deps` binaries and run cargo with `CARGO_INCREMENTAL=0`.

## Milestone ladder (pre-respec)

M2 (cuts A + B) -> M3a (docs-first broker-1d REST grants) -> M3b (code) ->
M4 (ratchets incl. the main.rs string) -> M5 (hostile + refuter reviews to
2 consecutive clean rounds + verifier matrix rounds).

## M2 worklist (as prepared)

Read-only recon, 2026-07-21. Line anchors below are against `origin/main`
`c1d637281cdd5a3a33c778b01e9377077e94e15c` (the rebuild base).

### SHAs

| Ref | SHA | Note |
|---|---|---|
| `origin/claude/tf-diet-11-broker-1d` (tip at recon time) | `ce86a5a543029b2345822db8f05142c25c711b61` | plan-shell commit ONLY (adds the plan file; no code) |
| `origin/main` (rebuild base) | `c1d637281cdd5a3a33c778b01e9377077e94e15c` | all line anchors below are against THIS tree |

The approved plan (`.claude/plans/active-plan-tf-diet-11-broker-1d.md`, Status
APPROVED, dated 2026-07-20 with the two verbatim operator quotes) is committed on
this branch — its plan-shell form is recoverable at `ce86a5a`, and the current form
rides the preservation commit. Contract summary: exactly 11 frames
`1m, 2m, 3m, 4m, 5m, 15m, 1h, 2h, 3h, 4h, 1d` (retire 6m..14m + 30m); 1d never
live-aggregated (broker-pulled per feed, feed-tagged, DEDUP-idempotent);
keep-as-history for retired tables; removal completeness enforced by a
build-failing source-scan ratchet.

### M2a — TfIndex 21 -> 11 (drop the 6m..14m band; ordinal repack)

| Anchor | What |
|---|---|
| `crates/trading/src/candles/tf_index.rs:29` | `pub const TF_COUNT: usize = 21;` -> 11 |
| `crates/trading/src/candles/tf_index.rs:56` | `#[repr(u8)] enum TfIndex` — old M1=0..M5=4, M6=5..M14=13, M15=14, M30=15, H1=16..H4=19, D1=20; new M1=0..M5=4, M15=5, H1=6..H4=9, D1=10 |
| `crates/trading/src/candles/tf_index.rs:108` | `TfIndex::ALL: [TfIndex; TF_COUNT]` — shrink to 11 |
| `crates/trading/src/candles/tf_index.rs:276` | label arms — delete M6..M14 (+ M30 in M2b) in every match (table_name / display / seconds-per-bucket / from_ordinal) |
| `crates/trading/src/candles/tf_index.rs:485` | 21-label list in the test/parse table |
| `crates/trading/src/candles/tf_index.rs:362,396` | in-file `TF_COUNT == 21` asserts -> 11-pin (`test_tf_count_is_eleven`, ordinal-table pin) |
| `crates/trading/src/candles/mod.rs:46` | re-export site (unchanged unless INTRADAY_TFS is exported here too) |

### M2b — 30m removal (OWN isolated commit — 30m-only anchors)

| Anchor | What |
|---|---|
| `crates/trading/src/candles/tf_index.rs:276` | `Self::M30 => "30m"` arm (+ its ordinal/mapping arms) |
| `crates/app/src/metrics_catalog.rs:183` | `Self::M30 => "30m"` in `Tf::as_static_str` |
| `crates/app/src/metrics_catalog.rs:313` | `["1m","5m","15m","30m","1h","2h","3h","4h","1d"]` label array/test |
| `crates/common/src/config.rs:2276` | `"30m".to_string()` in `TimeframesConfig::default_list()` |
| `crates/common/src/config.rs:4952` | test `expected: Vec<&str>` with "30m" |
| `crates/common/tests/tf_symmetry_guard.rs:30` | `PR517_CANONICAL_TF_SET` contains "30m" |
| `crates/storage/src/shadow_persistence.rs:338` | 9-label array with "30m" |
| `crates/storage/src/shadow_persistence.rs:418` | `super_minute = ["30m","1h","2h","3h","4h","1d"]` |
| `crates/storage/src/shadow_persistence.rs:905` | 9-label test array |
| `crates/app/src/tf_consistency_boot.rs:3055` | test fixture row `["30m",...]` |
| `config/base.toml:368+` | `[engine.timeframes]` list entry "30m" |

### M2c — INTRADAY_TFS (`[TfIndex; 10]` = ALL minus D1) + fold-loop switch

| Anchor | What |
|---|---|
| `crates/trading/src/candles/tf_index.rs` (new const near :108) | `pub const INTRADAY_TFS: [TfIndex; 10]` |
| `crates/app/src/rest_candle_fold.rs:114` | imports TF_COUNT (add INTRADAY_TFS import) |
| `crates/app/src/rest_candle_fold.rs:395` | `buckets: [Option<TfBucket>; TF_COUNT]` (stays TF_COUNT-sized; D1 slot never populated) |
| `crates/app/src/rest_candle_fold.rs:451,510,530` | `for tf in TfIndex::ALL` fold/seal/force-seal loops -> INTRADAY_TFS |
| `crates/app/src/rest_candle_fold.rs:1356` | refold loop -> INTRADAY_TFS |
| `crates/app/src/rest_candle_fold.rs:2336,2608` | catchup loops -> INTRADAY_TFS (catchup must NOT re-fabricate 1d — test-pinned) |
| `crates/app/src/main.rs:~2487` | `info!` string "re-folds the stored month into all 21 timeframes (...)" -> reword to the 10-intraday/11-set truth |

### M2d — seal-spill format-version bump

| Anchor | What |
|---|---|
| `crates/storage/src/seal_spill.rs:1-70` | 128-byte wire-format doc: tf ordinal at byte 5 (`0..=20`), feed_index byte 6, byte 7 = padding (natural home for a version field), bytes 120-128 reserved. NO format-version constant exists today (grep verified empty) — introduce one |
| `crates/storage/src/seal_spill.rs` (from_ordinal/as_ordinal sites) | old ordinals 0-20 overlap new 0-10 with DIFFERENT meanings (old 5 = 6m decodes as new 15m) -> refuse + quarantine old-version records, never re-map |
| `crates/trading/src/candles/seal_ring.rs:475-480` | seal-record construction feeding spill (ordinal source) |

### M2e — storage DDL / writer / retention sweep (KEEP history)

| Anchor | What |
|---|---|
| `crates/storage/src/shadow_persistence.rs:129` | `candle_table_names() -> [&'static str; TF_COUNT]` -> the 11 names |
| `crates/storage/src/shadow_persistence.rs:338,418` | label arrays (see M2b) — become the 11-set / super-minute subset |
| `crates/storage/src/shadow_persistence.rs:369` | `RETIRED_QUESTDB_TABLES: [&str; 18]` — DO NOT add candles_6m..14m/candles_30m (the DROP loop at :599 drops at boot; keep-as-history forbids). Len-18 assert at :947 stays 18 |
| `crates/storage/src/shadow_persistence.rs:825-826` | asserts `TF_COUNT == 21` -> 11 |
| `crates/storage/src/shadow_candle_writer.rs:702,1096,1102,1138` | `for tf in TfIndex::ALL` ILP writer loops (auto-resize via TF_COUNT; fix the "21 plain candles_<tf>" doc header) |
| `crates/storage/src/partition_manager.rs:44-45,288,665-671` | candles_* retention/name sweep — retired tables LEAVE these lists |
| `crates/storage/src/partition_archive.rs:165,1843,2428` | archive lists + hot_window (14d) sweep sites |
| `crates/storage/src/shadow_seal_columns.rs:265,311-314` | seal-column TF mapping sites |
| `crates/storage/src/spot_bar_store.rs:53,329` | `[; TF_COUNT]` arrays (auto-resize) |
| (test) partition_retention_coverage_guard | retention coverage list must match the final set (carries the BARE-literal count pin — see Pitfalls) |

### M2f — TimeframesConfig + base.toml

| Anchor | What |
|---|---|
| `crates/common/src/config.rs:2235,2244` | `TimeframesConfig` field + struct |
| `crates/common/src/config.rs:2263+` | `default_list()` — current 9-set -> the 11-set `["1m","2m","3m","4m","5m","15m","1h","2h","3h","4h","1d"]` |
| `crates/common/src/config.rs:4926,4937,4951,4952,4958` | tests incl. expected-vec + count |
| `config/base.toml:368+` | `[engine.timeframes]` list -> 11-set |
| `config/base.toml:401` | `[cross_verify] timeframes_intraday = ["1m","5m","15m"]` — still a valid subset; leave alone |

### M2g — tf_symmetry_guard -> 11 + the 4 symmetry surfaces

| Anchor | What |
|---|---|
| `crates/common/tests/tf_symmetry_guard.rs:29-30` | `PR517_CANONICAL_TF_SET` (9) -> the 11-set; rename/redoc as the TF-diet canonical set |
| same file | `timeframes_config_default_matches_pr517_canonical_set` -> eleven-pin; `timeframes_config_default_has_no_retired_pr517_tfs` currently BANS 2m/3m/4m — must be rewritten to ban ONLY 6m..14m + 30m; `timeframes_config_default_count_is_9` -> count_is_11; the seconds-TF ban stays |
| `crates/app/src/metrics_catalog.rs:145` | `pub enum Tf` (9 variants) -> 11 (add M2,M3,M4; drop M30); `Tf::ALL: [Self; 9]` -> 11; `allowed_tf_labels() -> [&'static str; 9]` at :193+ -> 11; the :313 array |
| cascade_fanout + materialized_views | named by the guard's doc header as the other two lock-step surfaces — greps under those exact names returned empty on main; locate the real current spellings at build time or the cross-surface pin is vacuous |

### Risk list — guards/hooks/tests that scan TF counts or labels

1. tf_symmetry_guard `no_retired_pr517_tfs` FIRES on the 11-set — it bans 2m/3m/4m
   today; rewrite it in the SAME commit as the config default_list flip or CI is red.
2. Count pins everywhere: config-side `count_is_9` (tf_symmetry_guard +
   config.rs:4952-4958), enum-side `TF_COUNT==21` (tf_index.rs:362,396;
   shadow_persistence.rs:825-826), `RETIRED_QUESTDB_TABLES` len==18
   (shadow_persistence.rs:947 — stays 18, do not touch).
3. Seal-spill ordinal overlap is silent-corruption-class: old 0-20 vs new 0-10
   (old 5=6m decodes as new 5=15m). The format-version bump + refuse/quarantine is
   MANDATORY before any new-ordinal writer ships; no version field exists yet
   (byte 7 padding is the candidate home).
4. Keep-as-history invariant: retired candles_* tables leave DDL/retention/writer
   lists but must NEVER enter `RETIRED_QUESTDB_TABLES` (the boot-DROP list). A
   "cleanup" reflex here destroys history.
5. Ordinal==index SEMVER break: `[Mutex<LiveCandleState>; TF_COUNT]` +
   `[Sender; TF_COUNT]` arrays key on ordinals; every persisted/spilled ordinal from
   the 21-era is invalid post-repack — only seal-spill persists ordinals (verified),
   so the version bump confines the blast radius.
6. 30m isolated commit ordering: M2b anchors interleave with M2a files — land the
   6m..14m band drop first, then the 30m drop as its own commit touching the SAME
   files again, so each commit compiles + tests green independently.
7. Literal "21"/"9" strings in logs + docs: main.rs:~2487 info! ("all 21 timeframes"),
   shadow_candle_writer.rs doc header ("21 plain candles_<tf>"), tf_index.rs /
   shadow_persistence.rs comments (the only 2 files with literal `; 21]`, mostly
   comments — real arrays use `[; TF_COUNT]`). The tf_diet_guard source-scan (plan
   Item 5) also catches stale retired labels.
8. Two unlocated symmetry surfaces: `cascade_fanout::CascadeFanout` +
   `materialized_views::VIEW_DEFS` (named by the guard doc header) did not grep under
   those names on main — find their real spellings before plan Item 3 or the guard's
   cross-surface pin is vacuous.
9. Pre-push hooks: test-count ratchet (fine — tests only increase), banned-pattern
   scan, plan-gate (plan exists + APPROVED at the branch tip — rebuild commits must
   land on the branch carrying the plan file), boot-symmetry guard (S6-G3+G4) may
   react to new spawn sites in plan Item 4 (out of M2 scope but same branch).
10. cross_verify intraday list (base.toml:401) uses only 1m/5m/15m — remains a valid
    subset; do not "helpfully" expand it (scope creep).
