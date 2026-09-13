//! Ensure-DDL boot wiring guard (Track A, 2026-07-18).
//!
//! The PR-C2 (#1522) Dhan-lane deletion and the #1581 Groww live-feed
//! deletion removed the ONLY call sites of the candle-table DDL chain
//! (`drop_legacy_candle_objects` → `ensure_shadow_candle_tables` →
//! `ensure_named_views`) while the REST-era bar-fold KEPT writing the
//! `candles_*` tables — so a fresh QuestDB volume auto-created them via
//! ILP WITHOUT `DEDUP ENABLE UPSERT KEYS` (silent duplicate-row window).
//!
//! This build-failing source-scan pins the re-wire so a future refactor
//! cannot silently re-open the class:
//!
//! 1. `build_shared_infra` in main.rs awaits
//!    `candle_ddl_boot::run_candle_ddl_at_boot` BEFORE the
//!    `spawn_seal_writer_loop` call (DDL-before-first-ILP ordering).
//! 2. `candle_ddl_boot.rs` actually calls the three storage DDL fns in
//!    the load-bearing drop → ensure → views order (stub-guard).
//! 3. Every LIVE-writer table's ensure fn keeps ≥1 production call site
//!    (the writer → table → ensure → boot-call-site coverage table).

use std::path::Path;

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read_src(rel: &str) -> String {
    let path = manifest_dir().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Production region = everything above the first column-0 `#[cfg(test)]`
/// line (the house source-scan convention), so test-module mentions can
/// never satisfy a production pin.
fn production_region(src: &str) -> String {
    match src.find("\n#[cfg(test)]") {
        Some(idx) => src[..idx].to_string(),
        None => src.to_string(),
    }
}

#[test]
fn test_candle_ddl_boot_awaited_before_seal_writer_spawn_in_main() {
    let main_src = production_region(&read_src("src/main.rs"));

    let ddl_call = "candle_ddl_boot::run_candle_ddl_at_boot(&config.questdb).await";
    let ddl_pos = main_src.find(ddl_call).unwrap_or_else(|| {
        panic!(
            "main.rs production region must await `{ddl_call}` inside \
             build_shared_infra — the fresh-volume no-DEDUP fix (Track A 2026-07-18)"
        )
    });
    assert_eq!(
        main_src.matches(ddl_call).count(),
        1,
        "exactly ONE candle-DDL boot await site (double execution would \
         double the 60s down-QuestDB probe stall)"
    );

    // The CALL site (not the fn definition) — the argument form is unique
    // to the build_shared_infra call.
    let seal_call = "spawn_seal_writer_loop(&config.questdb);";
    let seal_pos = main_src.find(seal_call).unwrap_or_else(|| {
        panic!("main.rs production region must call `{seal_call}` in build_shared_infra")
    });

    assert!(
        ddl_pos < seal_pos,
        "run_candle_ddl_at_boot must be awaited BEFORE spawn_seal_writer_loop \
         installs the global seal sender — the candle DDL (with DEDUP) must \
         land before the first fold seal can reach ILP"
    );
}

#[test]
fn test_candle_ddl_boot_calls_drop_ensure_views_in_order() {
    let boot_src = production_region(&read_src("src/candle_ddl_boot.rs"));

    let drop_pos = boot_src
        .find("shadow_persistence::drop_legacy_candle_objects(questdb).await")
        .expect("candle_ddl_boot must run the retired-object drop sweep");
    let ensure_pos = boot_src
        .find("shadow_persistence::ensure_shadow_candle_tables(questdb).await")
        .expect("candle_ddl_boot must ensure the 21 candle tables (DEDUP)");
    let views_pos = boot_src
        .find("console_views::ensure_named_views(questdb).await")
        .expect("candle_ddl_boot must ensure the analyst console views");

    assert!(
        drop_pos < ensure_pos && ensure_pos < views_pos,
        "load-bearing order: drop legacy objects (free squatted names) -> \
         ensure candle tables -> named views (validate column refs)"
    );
}

/// The writer → table → ensure → boot-call-site coverage table: every
/// LIVE-writer table's ensure fn must keep at least the named production
/// call site. Losing one silently re-opens the fresh-volume no-DEDUP
/// window for that table.
#[test]
fn test_every_live_table_ensure_fn_keeps_its_boot_call_site() {
    // (ensure fn call needle, caller file relative to crates/app)
    let coverage: &[(&str, &str)] = &[
        // candles_* (21 tables) — seal chain writer (rest_candle_fold)
        ("ensure_shadow_candle_tables", "src/candle_ddl_boot.rs"),
        // analyst console views (read-only projections)
        ("ensure_named_views", "src/candle_ddl_boot.rs"),
        // spot_1m_rest — the spot legs
        ("ensure_spot_1m_rest_table", "src/spot_1m_rest_boot.rs"),
        ("ensure_spot_1m_rest_table", "src/cadence_boot.rs"),
        // option_chain_1m — chain legs
        (
            "ensure_option_chain_1m_table",
            "src/option_chain_1m_boot.rs",
        ),
        ("ensure_option_chain_1m_table", "src/cadence_boot.rs"),
        // ticks — the live lane's own table. Its 5-key DEDUP
        // (ts, security_id, segment, capture_seq, feed) is what makes a WAL
        // replay idempotent; an ILP-auto-created table has none of it.
        // Ensured through the bounded retry loop since 2026-09-08 — a single
        // fire-and-forget attempt left the key missing for a whole session.
        ("ensure_ticks_table", "src/candle_ddl_boot.rs"),
        // market_depth — the depth-20 + depth-200 common table (2026-08-15).
        // Its DEDUP key carries a `depth_kind` discriminator no other table
        // needs; an ILP-auto-created table arrives without it and the two
        // depth pools begin silently overwriting each other's levels. Same
        // retry loop as `ticks`, so neither is retried without the other.
        ("ensure_market_depth_table", "src/candle_ddl_boot.rs"),
        // top_volume_rank — the 1 s / 5 s volume-ranking snapshots
        // (2026-09-06). Written every second by its offload writer from the
        // first ranking sweep; until 2026-09-08 its ensure fn had ZERO
        // production callers, so a fresh volume would have let the first ILP
        // row auto-create it with none of its 6-key DEDUP
        // (ts, tf, family, feed, security_id, segment). Same retry loop as
        // `ticks` and `market_depth`.
        ("ensure_top_volume_rank_table", "src/candle_ddl_boot.rs"),
        ("run_live_table_ddl_at_boot", "src/main.rs"),
        // rest_fetch_audit — every REST leg's forensics
        ("ensure_rest_fetch_audit_table", "src/spot_1m_rest_boot.rs"),
        // tf_consistency_audit — 15:40 IST verifier
        (
            "ensure_tf_consistency_audit_table",
            "src/tf_consistency_boot.rs",
        ),
        // order_audit / pnl_audit — order-side observability consumers
        ("ensure_order_audit_table", "src/order_observability.rs"),
        ("ensure_pnl_audit_table", "src/order_observability.rs"),
        // ws_event_audit — SEBI-retentioned lifecycle forensics
        ("ensure_ws_event_audit_table", "src/ws_audit_consumer.rs"),
        // scoreboard tables — 15:45 IST aggregation
        (
            "ensure_feed_scoreboard_tables",
            "src/feed_scoreboard_boot.rs",
        ),
        (
            "ensure_feed_episode_audit_table",
            "src/feed_scoreboard_boot.rs",
        ),
        // (tick_conservation_audit's ensure row retired 2026-07-18 with the
        // tick-conservation audit modules — dead-WS sweep follow-up; the
        // TABLE itself is retained per SEBI 5y, no DDL drop anywhere.)
        // index_constituency — ts-pin boot migration (SEBI, never dropped)
        (
            "ensure_index_constituency_table",
            "src/index_constituency_boot.rs",
        ),
    ];

    for (needle, rel) in coverage {
        let src = production_region(&read_src(rel));
        assert!(
            src.contains(needle),
            "{rel} lost its production `{needle}` call — the fresh-volume \
             no-DEDUP duplicate-row window re-opens for that table \
             (Track A coverage table, 2026-07-18)"
        );
    }
}

/// Scanner self-test: the production-region split must actually excise a
/// test module so a `#[cfg(test)]`-only mention can never satisfy a pin.
#[test]
fn test_production_region_split_excises_test_modules() {
    let fixture = "fn prod() {}\n#[cfg(test)]\nmod tests { fn t() { needle(); } }\n";
    let region = production_region(fixture);
    assert!(region.contains("fn prod"));
    assert!(!region.contains("needle()"));
}

/// The candle DDL — which carries the LEGACY VIEW SWEEP — must be awaited
/// BEFORE the live-table DDL, which carries the `top_volume_rank` → `top_volume`
/// RENAME.
///
/// ## Why this ordering is load-bearing, and why nothing pinned it until now
///
/// A QuestDB view is stored as its SQL TEXT and resolved at query time, so the
/// four legacy `top_volume_rank_{1s,3s,5s,1m}` views must be dropped before the
/// base table they name is renamed away from under them. More sharply: whether
/// QuestDB REFUSES to rename a table that dependent views reference is
/// **UNVERIFIED** — no QuestDB was reachable when the rename was written. If it
/// does refuse, a rename attempted with those views still present fails on
/// EVERY boot forever, and every ranking row written before the rename stays
/// stranded in a table that is no longer in `HOUR_PARTITIONED_TABLES` and so is
/// never swept.
///
/// The two calls sit ~47 lines apart in one 4,000-line function. Swapping them
/// COMPILES GREEN and produces exactly the order the design says is unsafe, and
/// a 2026-09-13 adversarial sweep found that the existing guards in this file
/// pin only (a) candle-DDL before the seal-writer spawn, (b) the ordering
/// INSIDE `candle_ddl_boot.rs`, and (c) that the live-table DDL is called at
/// all. None of them relates the two to each other.
///
/// This is the class this repository keeps recording: a correct ordering held
/// by convention, in a file where the only thing preserving it is that nobody
/// has yet had a reason to move a line.
#[test]
fn the_legacy_view_sweep_is_awaited_before_the_top_volume_rename() {
    let main_src = production_region(&read_src("src/main.rs"));

    let sweep = "candle_ddl_boot::run_candle_ddl_at_boot(&config.questdb).await";
    let rename = "candle_ddl_boot::run_live_table_ddl_at_boot(&config.questdb).await";

    // COUNTED before compared. A `find` on a string that occurs zero times
    // would panic with a clear message, but one that occurs TWICE would pin the
    // first occurrence and silently ignore a second call site placed anywhere —
    // including one placed on the wrong side of the rename.
    assert_eq!(
        main_src.matches(sweep).count(),
        1,
        "main.rs must await the candle DDL exactly once in production; this \
         guard compares its position against the rename below, and two call \
         sites would make that comparison meaningless"
    );
    assert_eq!(
        main_src.matches(rename).count(),
        1,
        "main.rs must await the live-table DDL exactly once in production"
    );

    let sweep_pos = main_src
        .find(sweep)
        .expect("counted above, so this cannot fail");
    let rename_pos = main_src
        .find(rename)
        .expect("counted above, so this cannot fail");

    assert!(
        sweep_pos < rename_pos,
        "`run_candle_ddl_at_boot` (which drops the four LEGACY \
         top_volume_rank_* views) must be awaited BEFORE \
         `run_live_table_ddl_at_boot` (which RENAMES top_volume_rank -> \
         top_volume). Whether QuestDB refuses a rename with dependent views is \
         UNVERIFIED; dropping first removes that failure mode whether or not it \
         exists, and dropping second would not. Found the sweep at byte \
         {sweep_pos} and the rename at byte {rename_pos}."
    );
}

/// The `top_volume` view re-ensure must NOT be gated on the tick or depth DDL.
///
/// It sat inside `if ticks_ok && depth_ok && rank_ok` until 2026-09-13, which
/// made the remedy hostage to two unrelated tables: a `ticks` DDL that failed
/// every attempt skipped the view pass even though `top_volume` had been
/// created, leaving the four views absent for the whole session — precisely the
/// failure the re-ensure exists to fix.
#[test]
fn the_view_reensure_is_gated_on_the_rank_table_alone() {
    let boot = production_region(&read_src("src/candle_ddl_boot.rs"));

    let gate = boot.find("if rank_ok && !views_reensured {").expect(
        "the view re-ensure must be gated on `rank_ok` ALONE (plus its \
             once-per-boot latch), never on ticks_ok/depth_ok — coupling a fix \
             to conditions it does not depend on makes it unavailable in the \
             case it was written for",
    );
    // Searched from the GATE onward, not from the start of the file.
    // `run_candle_ddl_at_boot` makes the FIRST `ensure_named_views` call ~75
    // lines earlier, so a plain `find` locates that one and then "proves" the
    // gate comes after the call it is supposed to guard. The guard was looking
    // at the wrong occurrence of a string that legitimately appears twice.
    assert!(
        boot[gate..].contains("console_views::ensure_named_views(questdb).await"),
        "the `rank_ok` gate must PRECEDE the re-ensure call it guards — no \
         re-ensure call was found after the gate"
    );

    // And the success return stays gated on all three: reaching it means every
    // live table is ready, which is a different claim from "the views were
    // re-attempted".
    assert!(
        boot.contains("if ticks_ok && depth_ok && rank_ok {"),
        "the boot-complete return must still require ALL THREE tables — \
         decoupling the view pass must not also weaken what `true` means"
    );
}

/// EVERY offload writer thread the lane spawns must be joined at shutdown and
/// must have a seeded `writer=` label on the abandonment counter.
///
/// ## The gap this closes
///
/// The lane spawns THREE writer threads — ticks, depth, and (since 2026-09-07)
/// top-volume snapshots. The first two have had shutdown accounting since
/// 2026-08-28: the queue is closed, the thread is joined against a shared
/// deadline, and a timeout or panic increments
/// `tv_offload_writer_shutdown_incomplete_total{writer=...}` beside a coded
/// error. §2.3n of the noise lock was written for exactly that, in its own
/// words because "a thread that never got to run its drain increments neither"
/// loss series.
///
/// The third writer simply never joined the pattern. It was spawned with
/// `Ok(_handle) =>` — the handle discarded — so at every 17:30 stop and every
/// mid-session redeploy up to `TOP_VOLUME_FLUSH_QUEUE_DEPTH` batches plus the
/// one in flight were abandoned with no counter, no log and no alarm. The
/// magnitude grew on the branch that removed the top-250 persistence cut, which
/// made a batch market-bounded rather than <= 500 rows.
///
/// This guard is deliberately about SYMMETRY rather than about the third
/// writer: the defect was not that someone forgot a join, it was that nothing
/// noticed a new writer arriving without one. A fourth writer now fails the
/// build until it is accounted for.
#[test]
fn every_offload_writer_is_joined_and_labelled_at_shutdown() {
    let lane = production_region(&read_src("src/dhan_feed_stack.rs"));

    // The labels that must be SEEDED, so the CloudWatch agent's
    // dropped-first-sample rule cannot swallow a series on the one day it
    // fires. An unseeded counter publishes NOTHING the first time it moves.
    for writer in ["tick", "depth", "top_volume"] {
        let seed = format!(
            "metrics::counter!(OFFLOAD_SHUTDOWN_INCOMPLETE_COUNTER, \"writer\" => \"{writer}\").increment(0)"
        );
        assert!(
            lane.contains(&seed),
            "the `{writer}` abandonment series must be SEEDED at zero at \
             construction. The agent drops the first sample of a series it has \
             never seen, so a counter seeded only by its own first event \
             reports nothing on the one day it matters"
        );
    }

    // A spawned writer whose handle is DISCARDED cannot be joined. The exact
    // spelling that hid the third writer was `Ok(_handle) =>`.
    //
    // Asserted POSITIVELY — that the handle is bound and handed over — rather
    // than by banning the discard spelling. The first draft did ban the string,
    // and it went red against CORRECT code: the fix's own explanatory comment
    // quotes `Ok(_handle)` to say what it replaced, and a source scan cannot
    // tell prose from code. A guard that forbids naming the defect it prevents
    // makes the defect undocumentable, which is a bad trade for a pin that the
    // positive form gives anyway.
    assert!(
        lane.contains("with_top_volume_writer(producer, handle)"),
        "the top-volume writer's JoinHandle must be BOUND at the spawn site and \
         handed to the lane with its producer. Discarding it (`Ok(_handle) =>`) \
         means the thread can never be joined at shutdown, so its final batches \
         die with the process and nothing counts them — the state this writer \
         shipped in for six days while its two siblings had the accounting"
    );

    // All three joins must be present, and against the SAME deadline: `main`
    // gives the whole task ONE flush budget, so independent graces would SUM
    // past it -- the arithmetic the compile-time assert in main.rs keeps honest.
    for join in [
        "ingest.shutdown_offload_writer(offload_deadline)",
        "ingest.shutdown_depth_offload_writer(offload_deadline)",
        "ingest.shutdown_top_volume_writer(offload_deadline)",
    ] {
        assert!(
            lane.contains(join),
            "the shutdown tail must call `{join}` -- every offload queue is \
             closed before any join begins, so the three threads drain in \
             parallel and the total wait is the MAX of the three, never the sum"
        );
    }
}
