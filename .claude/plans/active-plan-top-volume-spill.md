# Implementation Plan: top_volume save-to-disk-and-replay tier

**Status:** APPROVED
**Date:** 2026-10-13
**Approved by:** Parthiban, 2026-10-13 ~17:10 IST, verbatim: "Approve dude don't miss anything dude okay?" (approves every plan default for Q1–Q5 as put to him in chat; see "Owner decisions" at the end)
**Crates:** tickvault-storage, tickvault-app (no change to tickvault-common, core, trading, api)
**Feed scope:** Dhan only (owner rule 2026-10-13, verbatim: "Bro never ever pick or use any groww or Truedata dude justbsue one and only dhan dude okay?"). Nothing here touches another feed; the spill file name carries `dhan` because `top_volume` rows are stamped `feed=dhan` (`SNAPSHOT_FEED`).

> **⚠ Active-plan cap.** Before this file was written there were already **5**
> `.claude/plans/active-plan*.md` files (audit-fixes-2026-09-26, dhan-order-surface,
> feed-hardening, gdf-feed, greeks-trading-core). This one makes **6**, which is
> over `PLAN_GATE_MAX_ACTIVE` = 5, so `plan-gate.sh` (V7) will BLOCK every
> implementation push until one is archived or this plan is folded into an
> existing one. Recommended: fold this in as a new item of
> `active-plan-feed-hardening.md`, or archive `active-plan-gdf-feed.md` (GDF is
> not Dhan, and the owner's 2026-10-13 rule says only Dhan). Owner decides.

> **Guarantee matrices:** the 15-row and 7-row matrices from
> `.claude/rules/project/per-wave-guarantee-matrix.md` are instantiated for this
> plan at the end of this file.

> **How this plan was made (honest):** the request asked for three parallel
> subagents (machinery mapper, top_volume path mapper, hostile reviewer). This
> session had no agent-spawning tool, so the three roles were run one after
> another by one agent, each against the source. Every file:line below was read
> in this session (Verified). The hostile-review findings and how the plan
> answers each are in the "Hostile review" section. CloudWatch and the local
> QuestDB were both unreachable from this session, so the split of today's
> 10,277 lost rows between the two loss arms is **Unknown** (see Open questions).

---

## Problem (measured by the owner, 2026-10-13)

10,277 rows of the busiest-contract boards (`top_volume_1s/3s/5s/1m`) were lost
between 09:22 and 11:36 IST because QuestDB timed out. Ticks, depth and candles
each have a disk tier; `top_volume` has none, by an explicit 2026-09 decision
("a snapshot is a periodic sample, not a unique event"). That premise is wrong
for an audit table: a lost window is never re-ranked, so the hole is permanent.

## How the write path works today (Verified, read in source)

```
frame drain (tokio task)                        tv-top-volume-writer (OS thread)
  sweep step stages rows ──► TopVolumeRowProducer ──try_send──► rx.recv()
  (top_volume_rank_persistence.rs:2234 stage,    queue depth 5   TopVolumeRowWriter::write_batch (:2367)
   :2256 hand_off)                               (:2147)           format ILP per row (append_row :1217)
                                                                   TopVolumeRankWriterSink::write (:1966)
                                                                     flush, one fast retry
                                                                     FAIL -> report_lost (:2016): counted, gone
```

Two places drop rows, both counted on `tv_top_volume_rank_rows_discarded_total`
(already paged through the `market-data-refused` loss group,
`deploy/aws/terraform/loss-group-alarms.tf:63`):

| # | Where | When | Today |
|---|---|---|---|
| L1 | writer thread, `TopVolumeRankWriterSink::write` → `report_lost` (`:2016`) | QuestDB flush fails (timeout, reset after retry, no sender) | rows dropped |
| L2 | drain, `TopVolumeRowProducer::hand_off` `QueueFull` (`:2278`) | writer thread is behind | whole sweep dropped |
| L3 | writer thread, unkeyed gate (`:1974`) | tables not yet given their DEDUP key | rows dropped |

**Why a timeout causes L2 as well as L1 (Verified from repo text, magnitude Assumed):**
the sender is built with `request_timeout=5000` (`:987`), and the repo's own rule
text (`no-rest-except-live-feed` §12.15.8) records an ILP flush's worst case as
"5 s + bytes at 100 KiB/s". A typical 1 s board (~2,000 rows × ~330 B ≈ 660 KB,
Assumed) can therefore hold the writer thread ~11–12 s per failed flush, while
the drain hands it a new sweep every second. The 5-deep queue fills in about 5 s
and every later sweep is dropped at the drain (L2). **A disk tier that only
catches L1 would miss most of an outage.** This is the main finding of the
hostile pass and it shapes the design.

## How ticks and depth survive the same failure (Verified)

| Piece | Where | Reused here? |
|---|---|---|
| Append a failed ILP payload verbatim to `data/spill/<tier>/<prefix>-<feed>-<unix hour>.ilp`, synced before it counts | `tick_persistence.rs:1416` `spill_failed_ilp`; sync `:1371`; depth twin `depth_persistence.rs:1007` | YES — generalised (item 1), not copied |
| Size rail + free-space floor as pure classifiers | `classify_spill_ceiling` `:1163`, `classify_spill_floor` `:1240`, `BlindWritePolicy` `:1185` | YES, unchanged |
| Tier priority as reserves (ticks 16 GiB < depth 32 GiB) | `DEPTH_SPILL_FREE_RESERVE_BYTES` `:1118` + compile assert `:1121` | YES — top_volume gets the largest reserve (item 2) |
| Writer-thread rescue on a failed flush | `TickWriterSink::rescue` `:4136` | YES — same shape on the new flush thread (item 3) |
| Queue-full → rescue off the drain | `split_rescue_offload` `:3072`, H4 park `:3134` | Shape reused on the formatter thread (item 3); the drain itself is not touched |
| Replay: stream file in 32 MiB reads, POST 8 MiB line-aligned chunks to `/write`, resume offset per file, line isolation + quarantine on permanent refusal, drain-first-then-every-300 s, supervised | `tick_spill_replay.rs:785` `replay_spill_dir`, `:1457` `run_replay_loop`, `:1609` `spawn_supervised_tick_spill_replay`; spawned twice in `app/src/main.rs:2969`, `:3001` | YES — a third spawn on the new directory (item 5) |
| Quarantine trim only after a verified cold copy | `prune_quarantine` `tick_persistence.rs:1797`; upload `raw_frame_upload.rs:1521` | YES (item 7) |
| Archive waits while a spill is pending | `partition_archive.rs:2452` `spill_dirs_have_pending_data`, used per table at `:2520` | YES, scoped to the top_volume tables only (item 6) |

---

## Design

### Shape after the change

```
frame drain  (UNCHANGED: same stage + one try_send per sweep, same 5-deep queue)
     │
     ▼
tv-top-volume-writer (existing thread, now "formatter")
     format batch into an ILP buffer (as today)
     ├── breaker CLOSED and flush queue has room ──try_send──► tv-top-volume-flush (NEW thread)
     │                                                           sink.write (flush + one fast retry, as today)
     │                                                           ├── ok → done
     │                                                           └── FAIL → spill buffer (synced) + open breaker
     └── breaker OPEN, or flush queue full, or tables not keyed
            → spill buffer (synced) on THIS thread
                                   │
                         data/spill/top_volume/top_volume-dhan-<unix hour>.ilp
                                   │
              replay supervisor #3 (tick_spill_replay, reused) ──POST /write──► QuestDB
              holds while tables unkeyed; by default holds 08:00–15:40 IST (owner question Q1)
```

The formatter never waits on the network, so it keeps pace with the drain (its
per-sweep cost is the ILP formatting it already does, measured 14,932 µs for a
20,220-row sweep at `:2050`, plus at most one synced file append). That is what
closes L2: the drain's queue is emptied at formatting speed whether or not
QuestDB answers.

### Items

- [x] **1. One spill-append function for every ILP tier (generalise, do not fork).** (PR-A, 2026-10-13: `tick_persistence.rs::spill_failed_ilp_to`, `IlpSpillSpec`, `SpillCeilingKind`, `SpillTier`, `tick_spill_spec`; `spill_failed_ilp` is now a wrapper. Departures from the text, each needed to keep ticks unchanged or to stay allocation-free: the spec also carries `feed` (file name), `ceiling_kind` (Soft{reserve} for ticks, Hard for top_volume), `drain_held` (the tick-only drain total, `None` elsewhere), and the tier is an enum `SpillTier` rather than a `&'static str` so the blind-probe counter keeps a literal label (a non-literal label allocates). `tick_spill_spec(dir, feed)` is a builder, not a `TICK_SPILL_SPEC` const, because the tick dir and feed vary per writer. Guards re-pointed: `spill_rescue_sync_guard::both_spill_writers_sync_before_returning_ok`, `spill_ceiling_is_free_space_based_guard::the_tick_spill_ceiling_refuses_only_against_free_space`.)
  Turn `tick_persistence::spill_failed_ilp` into `spill_failed_ilp_to(spec: &IlpSpillSpec, payload, now, sync)`, where `IlpSpillSpec { dir, file_prefix, ceiling: fn() -> u64, free_reserve: u64, blind: BlindWritePolicy, tier_label: &'static str }`. The tick wrapper passes a `TICK_SPILL_SPEC` built from today's constants, so tick behaviour is byte-identical (pinned by the existing tests below). The drain-held cache (`TICK_DRAIN_SPILL_HELD`) stays tick-only and is consulted only when `sync` is `None`, exactly as today. Depth keeps its own `spill_failed_depth_ilp` in this plan (its framed `.dspl` format differs); folding it in is a separate follow-up.
  - Files: crates/storage/src/tick_persistence.rs
  - Tests: spill_failed_ilp_writes_the_payload_verbatim_so_it_can_be_replayed, spill_failed_ilp_appends_rather_than_truncating_a_second_failure, spill_failed_ilp_separates_feeds_and_hours, quarantined_bytes_count_toward_the_spill_ceiling, depth_never_spills_while_ticks_are_refused_including_when_df_is_blind (all existing, must stay green unchanged); new test_tick_spill_spec_reproduces_the_old_file_name_and_reserve

- [x] **2. The top_volume spill tier constants.** (PR-A, 2026-10-13: `top_volume_rank_persistence.rs::{TOP_VOLUME_SPILL_DIR, TOP_VOLUME_SPILL_FILE_PREFIX, TOP_VOLUME_SPILL_FEED, TOP_VOLUME_SPILL_VOLUME_FRACTION, TOP_VOLUME_SPILL_MIN_MAX_BYTES, TOP_VOLUME_SPILL_FREE_RESERVE_BYTES, top_volume_spill_ceiling_from_total, top_volume_spill_max_bytes, top_volume_spill_spec}` + two compile asserts; `tv_spill_free_probe_blind_total{tier="top_volume"}` seeded at 0 in `seed_spill_free_probe_blind_counters`. Not yet written to: the writer still drops until PR-B. Blind test is at the classifier level (the real probe cannot be made to fail in a unit test). Extra tests: `top_volume_never_spills_while_depth_is_refused_including_when_df_is_blind`, `test_only_the_tick_spec_is_a_soft_ceiling`, `test_top_volume_spill_spec_names_the_tier_and_never_uses_the_drain_total`, `test_top_volume_spill_ceiling_from_total_covers_every_arm`, `test_top_volume_spill_max_bytes_is_resolved_once_and_never_below_the_floor`, `test_spill_failed_ilp_to_appends_top_volume_batches_verbatim_and_synced`; tick side `test_tick_spill_spec_reproduces_the_old_file_name_and_reserve`, `test_spill_tier_labels_are_the_counter_labels`.)
  `TOP_VOLUME_SPILL_DIR = "data/spill/top_volume"`; file `top_volume-dhan-<unix hour>.ilp`; ceiling `top_volume_spill_max_bytes()` = volume total / 64 (half the tick fraction; floor 256 MiB), resolved once into a `OnceLock`, and a HARD ceiling (refuse past it whatever the free space, unlike the tick soft rail) because this is the lowest-priority tier; free reserve `TOP_VOLUME_SPILL_FREE_RESERVE_BYTES = DEPTH_SPILL_FREE_RESERVE_BYTES + 8 GiB` (40 GiB) with a compile-time assert that it is strictly above depth's; `BlindWritePolicy::FailClosed` (a blind probe refuses, like depth). Order of refusal as disk fills: top_volume first, then depth, then ticks.
  - Files: crates/storage/src/top_volume_rank_persistence.rs
  - Tests: test_top_volume_reserve_is_above_depth_reserve (compile assert + test), test_top_volume_spill_ceiling_is_hard, test_top_volume_spill_refuses_when_the_probe_is_blind

- [x] **3. Writer split into formatter + flush thread, with spill on every non-landing arm.** (PR-B, 2026-10-13: `top_volume_rank_persistence.rs::{TopVolumeRowWriter::write_batch/route/spill_buffer, TopVolumeFlushWorker::{run, flush_one, return_spare}, TopVolumeSpiller::spill, TopVolumeSpillArm, TopVolumeOffload, TopVolumeRankWriter::split_rows_for_offload(unlabelled, spill_dir)}`; `TopVolumeRankWriterSink::try_write` leaves the buffer intact on failure (the old `write` is now a dropping wrapper kept only for the legacy `split_for_offload` shape, which no production path builds); `dhan_feed_stack.rs`: `tv-top-volume-flush` spawned beside `tv-top-volume-writer`, `LiveIngest::with_top_volume_flush_thread`, `shutdown_top_volume_writer` polls both threads against the one deadline. Arms that spill: flush failed, flush queue full, tables not keyed (on both threads), no sender, breaker open, flush thread gone. `tv_top_volume_rank_rows_discarded_total` is incremented on these threads only inside `TopVolumeSpiller::spill`'s refusal arm (ratcheted by `test_spill_refusal_is_the_only_arm_that_counts_a_discard`). Local `/metrics` only: `tv_top_volume_rows_spilled_total{arm}` (six arms, seeded at 0). Departures from the text: a sixth arm `flush_thread_gone` and a fifth `breaker_open` were added; spare buffers return two deep (`TOP_VOLUME_SPARE_BUFFER_DEPTH`); `TopVolumeBatchOutcome.landed` is replaced by `queued/spilled/lost` (the formatter cannot know what landed). Extra tests: `run_returns_when_the_formatter_closes_its_queue`, `test_return_spare_keeps_a_normal_buffer_and_drops_a_burst_sized_one`, `test_spill_arm_labels_are_distinct_and_seeded`, `test_split_rows_for_offload_spills_into_the_folder_it_was_given`, `test_with_top_volume_flush_thread_holds_the_handle_for_shutdown`, conservation proptest `proptest_every_staged_row_is_landed_spilled_or_lost` (24 cases). ⚠ Rows spilled by PR-B are KEPT on disk in `data/spill/top_volume` and are NOT replayed until PR-C (item 5) lands the replay supervisor; until then they are re-ingestable by hand (`curl --data-binary @<file> http://<questdb>:9000/write`, idempotent by the DEDUP key). ⚠ NOT done in PR-B: the Rollback section's `[top_volume] spill_enabled` switch and `test_spill_disabled_restores_discard_behaviour` (needs a config field in tickvault-common; left for PR-C); the chaos test `chaos_top_volume_spill.rs`; the CLAUDE.md / scope-lock dated notes of item 8 (code comments were updated in this PR).)
  `TopVolumeRowWriter` keeps formatting; instead of calling `sink.write` inline it `try_send`s a `TopVolumeFlushBatch` to a new bounded queue (`TOP_VOLUME_FLUSH_QUEUE_DEPTH` = 4, the constant that already exists at `:1498`) consumed by a new `tv-top-volume-flush` thread that owns the `TopVolumeRankWriterSink`. Spill (synced, through item 1) replaces the drop in four arms: (a) sink flush failed after its retry (`report_lost`, `:2016`) — on the flush thread; (b) flush queue full — on the formatter thread; (c) no ILP sender — on the flush thread; (d) tables not keyed (`:1974`) — spill instead of discard, replay waits for the key (item 5). `tv_top_volume_rank_rows_discarded_total` is then incremented only when the spill itself is refused or fails, which is a real loss. Spent ILP buffers return to the formatter through a 2-deep spare lane (the tick `return_spare_buffer` shape, `tick_persistence.rs:4033`), dropping any buffer above `MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES` rather than pinning it.
  - Files: crates/storage/src/top_volume_rank_persistence.rs, crates/app/src/dhan_feed_stack.rs (spawn the second thread beside `tv-top-volume-writer`, ~:17616; join both in `shutdown_top_volume_writer`, ~:3494, against the same deadline)
  - Tests: test_failed_flush_spills_instead_of_discarding, test_full_flush_queue_spills_on_the_formatter_not_the_drain, test_unkeyed_batch_is_spilled_and_not_counted_as_discarded, test_spill_refusal_is_the_only_arm_that_counts_a_discard, test_flush_thread_gone_spills_on_the_formatter, test_shutdown_joins_both_threads_within_the_deadline

- [x] **4. A breaker so a dead QuestDB costs one timeout, not one per sweep.** (PR-B, 2026-10-13: `top_volume_rank_persistence.rs::{TopVolumeFlushBreaker::{is_open_at, record_failure_at, record_landed}, top_volume_breaker_cooldown_secs, TOP_VOLUME_BREAKER_BASE_COOLDOWN_SECS, TOP_VOLUME_BREAKER_MAX_COOLDOWN_SECS}`, one `Arc` shared by both threads. Departures: time is milliseconds since a monotonic `Instant` anchor in an `AtomicU64` (not a wall-clock `AtomicI64`), so a clock step cannot stretch a cooldown; a missing sender also trips it; the flush thread also checks it and spills a batch queued before it opened, so a dead QuestDB costs one timeout per cooldown even with batches already queued. Local `/metrics` only: `tv_top_volume_breaker_open` (gauge, 1 from a failed flush until the next landed one) and `tv_top_volume_breaker_trips_total`. Review round 1 (2026-10-13): the breaker trips only on a transport failure (`flush_failure_is_retryable`); a server refusal is spilled without tripping (`test_permanent_refusal_spills_without_tripping_the_breaker`). A seventh arm `shutdown`: `TopVolumeShutdown` is requested by `shutdown_top_volume_writer` before it polls, and both threads then spill instead of sending (`test_shutdown_request_spills_queued_and_new_batches_instead_of_sending`). Both threads' spillers share one lock and a failed append is truncated back (`IlpSpillSpec::truncate_failed_append`, top_volume only; `test_spills_are_serialised_and_a_failed_append_is_cut_back`). The abandon message names which thread and why.)
  One `AtomicI64` "spill until" instant shared by the two threads. A failed flush on the flush thread sets it to now + cooldown (30 s, doubling per consecutive failure to a 300 s cap, reset on the first landed flush). While it is in the future the formatter spills directly and never queues, so QuestDB is not hammered and the flush thread is idle. When it expires the next batch is queued as normal; that batch is the probe. Cost: one relaxed load per batch on the formatter.
  - Files: crates/storage/src/top_volume_rank_persistence.rs
  - Tests: test_breaker_opens_on_failure_and_formatter_spills_directly, test_breaker_cooldown_doubles_and_caps_at_300s, test_breaker_resets_after_a_landed_flush

- [ ] **5. Replay: reuse `tick_spill_replay`, add a per-directory policy.**
  Replace the two ad-hoc per-dir checks (`is_depth_spill_dir` for the in-session hold at `:1471`, `dir_label` for the clock at `:976`) with one `ReplayPolicy { window: Exchange | Arrival | None, hold: fn() -> bool, label: &'static str }` chosen from the directory. Ticks and depth map to exactly today's behaviour. `top_volume`: `window: None` (its writer has no session gate, so replay must not apply one — applying the exchange gate would, for example, delete a Muhurat-evening board replayed the next day, since `muhurat::current()` is per process), `label: "top_volume"` (today it would read `"other"`), `hold` = tables not yet keyed in this process (`TOP_VOLUME_TABLES_KEYED`, `:957`, is `pub(crate)` in the same crate) OR, by default, inside 08:00–15:40 IST (`depth_replay_held_at`) — see Q1. A third `spawn_supervised_tick_spill_replay(TOP_VOLUME_SPILL_DIR, …)` in `main.rs` beside the two at `:2969` and `:3001`; `spill_max_bytes_for_dir` (`:359`) learns the new dir for the quarantine trim.
  - Files: crates/storage/src/tick_spill_replay.rs, crates/app/src/main.rs
  - Tests: test_replay_policy_for_ticks_and_depth_is_unchanged, test_top_volume_replay_applies_no_window_filter, test_top_volume_replay_holds_until_tables_are_keyed, test_top_volume_replay_hold_window_matches_depth, test_replayed_top_volume_rows_upsert_onto_themselves (integration, mock `/write` that records bodies; re-POST of the same file yields identical bodies)

- [ ] **6. Archive gate, scoped per table.**
  `spill_dirs_have_pending_data` (`partition_archive.rs:2452`) takes the table: a `top_volume_*` table waits on `TOP_VOLUME_SPILL_DIR` only; every other table keeps today's ticks+depth check. So a top_volume spill can never defer the hour archive of `ticks` or `market_depth` (which would grow the disk), and a top_volume hour is not detached before its spilled rows are back.
  - Files: crates/storage/src/partition_archive.rs
  - Tests: test_top_volume_spill_defers_only_top_volume_tables, test_tick_and_depth_gate_unchanged

- [ ] **7. Zero-loss bookkeeping for the new folder.**
  Cold upload of `data/spill/top_volume/quarantine/` before any prune (`ColdFileSet::top_volume_quarantine`, a fourth pass in `run_spill_and_quarantine_passes`, `raw_frame_upload.rs:1521`, so Quote 27's "no market-data delete without a verified copy" holds); boot-time `prune_quarantine` call beside the tick one (`main.rs:2956`); confirm the existing `data/spill` disk-health watcher, retention sweep and `/api/debug/spill/status` see the subfolder (verify, change only if they do not).
  - Files: crates/storage/src/raw_frame_upload.rs, crates/app/src/main.rs
  - Tests: test_top_volume_quarantine_is_uploaded_before_prune, test_prune_refuses_an_unuploaded_top_volume_quarantine_file

- [ ] **8. Words and guards that say "no spill tier".**
  Correct, by dated note (house convention, never silent rewrite), the module header and the `TopVolumeOffloadOutcome::WidthCapped` / `report_lost` / spawn-site comments, the CLAUDE.md `top_volume_sweep.rs` row, and add a dated note under the 2026-09-12 section of `docs/claude-rules-full/project/websocket-connection-scope-lock.md` (its REJECT row "Removes the persistence bound entirely … the write path has no spill tier" stays: the bound is kept, only the drop becomes a spill). No new alarm (see Observability).
  - Files: crates/storage/src/top_volume_rank_persistence.rs, crates/app/src/dhan_feed_stack.rs, CLAUDE.md, docs/claude-rules-full/project/websocket-connection-scope-lock.md
  - Tests: none new beyond the guards already reading those files (`top_volume_stock_only_guard.rs`, `claude_md_codebase_map_guard.rs`) staying green

Suggested PR split (serial, one at a time): PR-A items 1–2 (storage only, no behaviour change for ticks); PR-B items 3–4; PR-C items 5–7; item 8 rides with whichever PR changes the behaviour it describes.

### What is deliberately NOT done

- **No change on the frame drain.** No new branch, allocation, disk call or lock per tick, per row or per sweep. The drain-side L2 drop arm stays as a last resort and is counted; the design makes it unreachable in an ordinary outage by keeping the formatter fast.
- **No WAL backing.** Top_volume rows are derived from the fold, not captured frames, so there is no capture sequence, no watermark floor and no rescue floor to hold. A crash loses rows still in RAM queues (stated under Failure Modes).
- **No change to the DEDUP key, the tables, the cadences, the ranking or the row content.**
- **No GDF/TrueData/Groww involvement.**

## Edge Cases

| # | Case | What happens |
|---|---|---|
| E1 | Same window replayed twice (crash between POST and offset record, or restart forgetting the in-RAM resume offset) | Every column of a row is fixed for its window and the DEDUP key `(ts, tf, family, feed, security_id, segment)` covers the identity, so the second write UPSERTs onto the first. Idempotent. |
| E2 | A spilled copy is replayed AFTER a live copy of the same key landed | Only possible if one window is ranked twice. Within one process the close clock reports each window once (`top_volume_sweep.rs` row in CLAUDE.md). Across a restart the new process's fold is fresh, so it does not rank the old process's closed windows. **Unknown:** whether the boot WAL refold can trigger board sweeps; if it can, a replayed spilled copy (computed with the full window) would overwrite a re-ranked copy (computed after a restart). Test `test_wal_refold_does_not_emit_board_rows` decides it in PR-C; if it fails, the plan adds a rule before PR-C merges. |
| E3 | Torn last line after a crash mid-append | Replay posts it; QuestDB refuses that one line; PR17 line isolation sets it aside in `quarantine/<file>.rejected-lines` and the rest lands (`tick_spill_replay.rs:598–637`). |
| E4 | Tables not keyed yet (boot DDL failed) | Rows spill (today: dropped). Replay holds until `TOP_VOLUME_TABLES_KEYED`, so ILP never auto-creates a table without its DEDUP key. If DDL never succeeds this process, files wait for the next process; kept, not lost. |
| E5 | Muhurat evening boards | No window filter on replay, so they survive a next-day replay. |
| E6 | Hour rollover while a file is open | New file per unix hour; replay never truncates the newest file and resumes by offset (existing behaviour, `:367–432`). |
| E7 | QuestDB flapping (alternating success/timeout) | Breaker opens per failure; probes at 30 s, 60 s … 300 s; each failing probe costs one flush timeout on the flush thread only, during which the formatter spills overflow. |
| E8 | Disk filling | Top_volume refuses first (40 GiB reserve, hard ceiling), before depth (32 GiB) and ticks (16 GiB). Refused rows are counted as discards and page through the existing loss group. |
| E9 | Very wide sweep (up to 50,000 rows, ~22.5 MB at the 450 B measured worst row) | One synced append of ~22.5 MB on the formatter or flush thread; never on the drain. |
| E10 | Row order within one `ts` | Replayed rows arrive later and out of order; readers must keep writing `ORDER BY ts, volume_percentage_change DESC`, which the module header already requires (order preservation was never verified). |
| E11 | Hour partition already archived and dropped under forced disk-pressure hours before replay | Replay would recreate that hour with only the spilled rows, and the next archive of that hour could overwrite the cold copy's latest version (versioning keeps the old one). **Risk**, see Q4. |

## Failure Modes

| # | Failure | Detect | Outcome |
|---|---|---|---|
| F1 | QuestDB times out / resets | flush error on the flush thread | Batch spilled (synced), breaker opens; formatter spills directly until cooldown. Rows not in QuestDB until replay; on disk. |
| F2 | Spill write refused (ceiling, reserve, blind probe) or fails (EIO) | `Err` from item 1 | Real loss: `tv_top_volume_rank_rows_discarded_total` += rows, coded `STORAGE-GAP-03` with `spill_error`, pages through `market-data-refused` (existing). |
| F3 | Flush thread panics or fails to spawn | `try_send` → Disconnected | Formatter spills every batch; one coded error at onset (powers of two, as today). |
| F4 | Formatter thread dies | drain `hand_off` → `SinkGone` | Unchanged from today: counted drop at the drain (no disk I/O on the drain). |
| F5 | Process killed (SIGKILL / power) | — | Lost: rows in the drain's staging vector, the 5-deep row queue, the 4-deep flush queue, and any bytes not yet synced. Not counted (same class as ticks' WAL queue at a kill). Bounded by ~10 batches. |
| F6 | Graceful shutdown with QuestDB down | shutdown deadline | Formatter drains its queue to disk at append speed; improves on today, where those batches were lost at the join. Abandonment past the deadline stays counted on `OFFLOAD_SHUTDOWN_INCOMPLETE_COUNTER{writer="top_volume"}`. |
| F7 | Replay supervisor dies | supervisor | Respawned after 30 s with `TICK-FLUSH-01` (existing). |
| F8 | QuestDB permanently refuses a file | 4xx | Whole file or single lines quarantined (existing), kept, uploaded cold before any prune (item 7). |

## Test Plan

- Unit (storage): every test named under items 1–7.
- Existing tick and depth spill/replay suites must pass unchanged (`cargo test -p tickvault-storage`), proving the generalisation in items 1 and 5 changed nothing for them.
- Integration (storage): mock HTTP `/write` server that (a) times out for N requests then accepts, (b) resets: assert zero discards, all rows eventually posted once per key, files emptied, offsets advanced.
- Chaos (new `crates/storage/tests/chaos_top_volume_spill.rs`): QuestDB down for a simulated 2-hour window at 4 cadences with 2,000 rows/sweep; assert drain-side `QueueFull` count stays 0 and every staged row is either landed or on disk.
- Property: proptest that for any interleaving of fail/ok flushes and queue-full events, `landed + spilled + discarded == staged` (conservation), and `discarded > 0` only when the spill was refused.
- DHAT: `dhat_top_volume_sweep` (existing, zero allocation across 1,000 sweeps on the drain) stays green unchanged; it is the proof the drain is untouched.
- Bench: none new — no hot-path code changes. N/A justified: drain untouched.
- Regression: `test_wal_refold_does_not_emit_board_rows` (E2).
- Scoped commands: `cargo test -p tickvault-storage`, `cargo test -p tickvault-app` (two crates touched; not `common`, so no workspace escalation).

## Rollback

- Config switch `[top_volume] spill_enabled = true` (default true once approved; `#[serde(default)]`). `false` restores today's behaviour exactly: the flush thread still exists but every non-landing arm discards, and the replay supervisor for the folder is not spawned. Tested by `test_spill_disabled_restores_discard_behaviour`.
- Code rollback is a revert of the PR; spilled files left on disk are plain ILP and stay replayable by the tick replay code or by hand (`curl --data-binary @file http://<questdb>:9000/write`), idempotent by the DEDUP key.
- Items 1 and 5 are behaviour-neutral refactors for ticks/depth; their revert is independent.

## Observability

No new alarm, no new EMF name, no new Telegram page (noise lock §2.10 forbids adding a counter to a loss group without a dated row; this plan does not need one: the paged counter `tv_top_volume_rank_rows_discarded_total` keeps its meaning "rows that will never reach the table", now only incremented on a refused spill).

Local `/metrics` only (each registered at 0 at construction):
- `tv_top_volume_rows_spilled_total{arm="flush_failed|queue_full|unkeyed|no_sender"}` — rows written to disk.
- `tv_top_volume_spill_bytes` — gauge, bytes held in the folder (from the replay round).
- `tv_top_volume_breaker_open` — gauge 0/1; `tv_top_volume_breaker_trips_total`.
- Replay reuses `tv_spill_replay_*` with `dir="top_volume"` (today it would read `"other"`).
- Logs: one coded `STORAGE-GAP-03` line per spill episode (powers of two), naming the file and the one-command re-ingest, like the tick rescue line; a `source = "rescued_to_spill"`-style source so a rescued batch is not mistaken for a loss in the errors stream.
- Proof query after an outage: `SELECT count() FROM top_volume_1s WHERE ts IN today()` before and after the replay round, plus `ls -l data/spill/top_volume` showing emptied files.

---

## Hostile review (run against the draft; each finding and its answer)

| # | Finding | Sev | Answer in the plan |
|---|---|---|---|
| H1 | Spilling only on flush failure misses most of an outage: the writer blocks ~5 s + bytes/100 KiB/s per failed flush, the drain's 5-deep queue fills, and sweeps are dropped AT THE DRAIN where no disk write is allowed. | CRITICAL | Items 3–4: formatter never blocks on the network; queue-full spills on the formatter thread; breaker stops repeated timeouts. Chaos test asserts drain `QueueFull` = 0. |
| H2 | Any disk write or new allocation on the frame drain would trade top_volume rows for ticks (Dhan skips a slow consumer with no sequence number). | CRITICAL | Drain code untouched; `dhat_top_volume_sweep` unchanged as proof. |
| H3 | Replay applying the tick exchange-time window drops Muhurat boards the next day; replay labelled `"other"` hides it in metrics. | HIGH | Item 5: `window: None`, label `top_volume`. |
| H4 | Replay before the DEDUP key exists makes ILP auto-create a keyless table → permanent duplicates. | HIGH | Item 5 hold on `TOP_VOLUME_TABLES_KEYED`; unkeyed rows spill and wait. |
| H5 | Adding the folder to the global archive gate would defer every table's hour archive while top_volume waits for the close, growing the disk. | HIGH | Item 6: per-table gate. |
| H6 | An unbounded or soft-ceiling tier for derived data can starve tick and depth rescue. | HIGH | Item 2: hard ceiling + 40 GiB reserve above depth, fail-closed when blind, compile-time ordering assert. |
| H7 | A third hand-written copy of the spill function would drift (depth already forked once). | MEDIUM | Item 1 generalises the tick function; depth fold-in recorded as follow-up. |
| H8 | "O(1)" claims: the spill append and replay are not O(1). | MEDIUM | Flagged honestly: spill is O(batch bytes) per failed batch, off the drain; replay O(file bytes) on its own task; ceiling check lists the folder O(files). Drain per-sweep cost unchanged (O(1) hand-off). |
| H9 | Crash loses queued rows silently. | MEDIUM | Stated as a limit (F5), same class as ticks. |
| H10 | Replayed rows can overwrite a re-ranked copy of the same window (E2), and replay into an already-dropped hour can overwrite a cold archive (E11). | MEDIUM | E2: decided by a test before PR-C. E11: owner question Q4. |
| H11 | Writing to rule/doc text that says "no spill tier" without a dated note breaks house convention and guards. | LOW | Item 8. |

---

## Guarantee matrix — 15 rows (per-wave-guarantee-matrix.md)

| Demand | This plan's proof |
|---|---|
| 100% code coverage | New code covered by the unit, integration, property and chaos tests above; storage floor 90.1 and app floor 72.6 (`quality/crate-coverage-thresholds.toml`) must not drop; coverage delta stated in each PR. |
| 100% audit coverage | The audit record IS `top_volume_<tf>`; this plan makes it survive outages. No new audit table. |
| 100% testing coverage | Unit, integration, property (conservation), chaos (2-hour outage), DHAT (existing drain gate), regression (E2). Loom N/A — two threads share one atomic with no ordering dependency beyond Relaxed timing. Bench N/A — no hot-path change. |
| 100% code checks | banned-pattern, pub-fn-test, pub-fn-wiring, plan-verify, secret-scan; every new pub fn has a caller in `dhan_feed_stack.rs` or `main.rs`. |
| 100% code performance | Drain unchanged (DHAT gate). Formatter adds ≤ one synced append per batch only while QuestDB fails. |
| 100% monitoring | Local counters/gauges listed under Observability; existing paged loss counter keeps its meaning. |
| 100% logging | Every spill and every refused spill logs `error!` with `code = STORAGE-GAP-03`, throttled to powers of two. |
| 100% alerting | Existing `market-data-refused` loss group already pages `tv_top_volume_rank_rows_discarded_total`; no new alarm needed or added. |
| 100% security | No secrets touched; spill files contain market-derived rows only; ILP payload is the writer's own escaped bytes (no injection surface added). |
| 100% security hardening | Attack-surface delta: one new local folder under `data/spill/`, same permissions as ticks/depth; no new network endpoint. |
| 100% bugs fixing | Hostile pass recorded above; full 3-agent review (hot-path, security, hostile) before and after each PR. |
| 100% scenarios covering | E1–E11 and F1–F8 each map to a named test or a stated owner question. |
| 100% functionalities covering | Every new pub fn gets a test and a call site. |
| 100% code review | 3-agent review per PR, before and after implementation. |
| 100% extreme check | Ratchets: compile-time reserve-ordering assert; conservation proptest; `test_spill_refusal_is_the_only_arm_that_counts_a_discard` fails the build if a drop arm reappears. |

## Guarantee matrix — 7 rows

| Demand | Honest envelope for this plan |
|---|---|
| Zero ticks lost | No tick path changes. Top_volume rows: zero loss inside the envelope "QuestDB down or slow for any length of time, disk above the 40 GiB reserve and under the top_volume ceiling, process not killed". Outside it: refused spills are counted and paged; a kill loses ≤ ~10 queued batches uncounted. |
| WS never disconnects | Untouched. |
| Never slow/locked/hanged | Drain untouched (DHAT gate). Formatter never waits on the network; flush thread waits ≤ one request timeout per probe. |
| QuestDB never fails | ABSORB: failed flush → synced spill → supervised replay; breaker keeps load off a struggling QuestDB. |
| O(1) latency | Drain hand-off stays O(1) per sweep. Spill is O(batch bytes) per failed batch, off the drain; replay O(file bytes) on its own task; ceiling check O(files in folder). Flagged, not relabelled. |
| Uniqueness + dedup | DEDUP key `(ts, tf, family, feed, security_id, segment)` unchanged; replay is idempotent; replay waits for the key to exist. |
| Real-time proof | Local counters + coded logs + the before/after count query; existing paged loss counter. |

Honest 100% claim: 100% inside the tested envelope, with ratcheted regression coverage: a QuestDB outage of any length is absorbed by a synced on-disk ILP spill and replayed by the supervised tick replay task, as long as free space stays above the 40 GiB top_volume reserve and the folder under its ceiling; beyond that, refused rows are counted on `tv_top_volume_rank_rows_discarded_total` and paged. A killed process can lose up to ~10 queued batches, uncounted.

## Disk and cost (labelled)

- Row size in the spill file: ~330 B typical (Assumed, from the 15-column ILP line), 450 B measured worst (`MEASURED_WORST_CASE_ILP_ROW_BYTES`, `:1869`).
- Normal day with no QuestDB trouble: **0 bytes** (nothing spills). Files are emptied after replay.
- Today's incident (10,277 rows): **≈ 3.4 MB** (≤ 4.6 MB worst).
- Same 2 h 14 m outage if every sweep failed, at the module's assumed 2,000 rows/sweep: 12,462 sweeps × 2,000 × 330 B ≈ **8.2 GB** (Assumed; the real rows/sweep has never been measured, see the module header).
- Whole session with QuestDB down: 35,805 sweeps × 2,000 × 330 B ≈ **23.6 GB** (Assumed), cut at the hard ceiling (1/64 of the volume, e.g. ~9.4 GB on a 600 GB volume) and the 40 GiB free reserve, past which rows are counted as lost.
- AWS cost: no new metric, alarm or log group; EBS unchanged. Cold-bucket bytes only for quarantined files (expected near zero).
- Live feed cost: none on the frame drain. RAM: up to 4 queued ILP buffers plus 2 spares on the writer side (~4 MB typical, ≤ ~135 MB at six 22.5 MB worst-case sweeps).

## Open questions for the owner

- **Q1.** Replay while the market is open, or only after 15:40? Default in this plan: after 15:40 (like depth), so a recovering QuestDB is not loaded with old boards during trading. Cost: the boards for the outage window are queryable only after the close.
- **Q2.** Priority on a filling disk: this plan refuses top_volume before depth and ticks. Agree?
- **Q3.** Hard ceiling 1/64 of the volume (~9.4 GB on 600 GB) — acceptable, or a different size?
- **Q4.** If disk pressure forces the hour archive before replay finishes, should the replay (a) still write those rows (they land in a new partition and are archived again later), or (b) set the file aside and keep it on disk for a manual decision? Plan default: (b), safer for the cold copy.
- **Q5.** Active-plan cap: fold this into `active-plan-feed-hardening.md`, or archive `active-plan-gdf-feed.md` (not Dhan) to make room?
- **Q6.** Can someone with box access read today's counters `tv_top_volume_rank_flush_queue_full_total` and the `STORAGE-GAP-03` lines? That tells us how the 10,277 rows split between drain drops (L2) and flush failures (L1); this session could not reach CloudWatch or QuestDB.

## Owner decisions (2026-10-13 ~17:10 IST)

The owner was shown a five-line summary and Q1–Q5 with this plan's defaults, and replied "Approve dude don't miss anything dude okay?". Recorded answers:

- **Q1:** replay only after 15:40 IST (plan default).
- **Q2:** on a filling disk, top_volume is refused before depth and ticks (plan default).
- **Q3:** hard ceiling 1/64 of the volume (plan default).
- **Q4:** (b) set the file aside and keep it on disk when its hour was archived first (plan default).
- **Q5:** archive `active-plan-gdf-feed.md` (not Dhan; owner rule 2026-10-13 "one and only dhan") to stay at the 5-plan cap. Done in PR-A: moved to `.claude/plans/archive/2026-07-13-gdf-feed.md`.
- **Q6 (answered from the live box, read-only CloudWatch, 2026-10-13):** both loss paths occurred. STORAGE-GAP-03 "the writer queue was full" (rows dropped before the writer thread): 7 coalesced episodes, 09:21:45–11:35:52 IST, per-line `dropped` summing to 2,645. "Could not flush buffer … timeout: per call" (rows discarded by the offload writer): 7 episodes, 11:14:29–11:26:16 IST, summing to 149. Lines are coalesced, so these sums are lower bounds, not the day total (Verified for the lines; the split of the 10,277 total is Assumed to be similar). The queue-full path dominates; items 3–4 cover both.
