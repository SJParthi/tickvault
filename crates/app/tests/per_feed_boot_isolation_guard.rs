//! Per-feed boot-isolation guarantee (operator lock 2026-06-23).
//!
//! Operator requirement (verbatim intent): *"until or unless the feed is ON,
//! the entire architecture related to that particular feed should never ever
//! be touched."* i.e. a feed that is OFF must trigger ZERO work — no auth, no
//! instrument fetch, no connection.
//!
//! This is a SOURCE-SCAN ratchet (same pattern as the existing
//! `secret_manager.rs::test_*_is_wired_into_main` guards): it reads the literal
//! `crates/app/src/main.rs` and asserts the per-feed boot gates are present, so
//! the build FAILS if a future edit removes the gate and lets an OFF feed start
//! authenticating / fetching instruments / connecting.
//!
//! It does NOT (and cannot) prove runtime behaviour by itself — the live boot
//! is exercised by the app's own boot path — but it mechanically prevents the
//! regression class "someone deleted the `if feeds.<x>_enabled` guard".

/// Read `crates/app/src/main.rs` regardless of the test's working directory.
fn read_main_rs() -> String {
    std::fs::read_to_string("src/main.rs")
        .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
        .expect("main.rs must be readable from the app crate test working dir")
}

#[test]
fn dhan_boot_is_gated_on_dhan_enabled() {
    let src = read_main_rs();
    // The Dhan lane (auth + daily-universe instruments + WS) must be wrapped by
    // a `dhan_enabled` gate, AND there must be an explicit skip branch for the
    // OFF case. Either spelling of the config access is accepted.
    let has_positive_gate =
        src.contains("feeds.dhan_enabled") || src.contains("config.feeds.dhan_enabled");
    assert!(
        has_positive_gate,
        "main.rs MUST gate the Dhan lane on `feeds.dhan_enabled` — removing it \
         would let an OFF Dhan feed authenticate / fetch instruments (operator \
         lock 2026-06-23: OFF feed = nothing touched)."
    );
    let has_skip_branch =
        src.contains("!config.feeds.dhan_enabled") || src.contains("!feeds.dhan_enabled");
    assert!(
        has_skip_branch,
        "main.rs MUST have an explicit `if !feeds.dhan_enabled` skip branch so \
         Dhan-OFF takes ZERO boot action (no auth, no instruments, no WS)."
    );
}

/// Read a Groww lane-module source regardless of the test's working directory.
fn read_app_src(file: &str) -> String {
    std::fs::read_to_string(format!("src/{file}"))
        .or_else(|_| std::fs::read_to_string(format!("crates/app/src/{file}")))
        .unwrap_or_else(|_| panic!("{file} must be readable from the app crate test working dir"))
}

#[test]
fn groww_lanes_spawn_dormant_and_self_idle_on_the_enable_flag() {
    // Operator 2026-06-24 (verbatim intent): "when I enable on/off the entire
    // mechanism and its architecture should run entirely right for that feed."
    //
    // The OLD model gated the Groww lane behind `if feeds.groww_enabled {…}` at
    // boot — so a feed OFF at boot could NEVER be cold-started by the webpage
    // toggle (DEGRADED, no lane behind the flag). The NEW model spawns all three
    // Groww lanes UNCONDITIONALLY at boot; each self-idles on
    // `is_enabled(Feed::Groww)` (a 2s poll, ZERO Groww work) while OFF — so the
    // OFF-feed-isolation guarantee (operator lock 2026-06-23: OFF feed = nothing
    // touched) is PRESERVED, just as a *dormant poll* rather than *not-spawned*.
    //
    // This guard proves the new invariant mechanically so a future edit cannot
    // (a) re-introduce the boot-gate that breaks the live toggle, OR
    // (b) drop the self-idle check that breaks OFF-feed isolation.
    let main = read_main_rs();

    // (1) All three lanes spawned UNCONDITIONALLY (not inside a groww_enabled if).
    for spawn in [
        "run_groww_bridge",
        "run_groww_sidecar_supervisor",
        "run_groww_activation_watcher",
    ] {
        assert!(
            main.contains(spawn),
            "main.rs MUST spawn `{spawn}` so the Groww lane exists for the live \
             toggle to cold-start (operator 2026-06-24: toggle runs the whole lane)."
        );
    }

    // (2) The spawns must be UNCONDITIONAL, proven positionally:
    //   (a) the spawn region (from the Groww-lane comment to the first spawn)
    //       contains NO `if …groww_enabled {` gate — the OLD model's regression; and
    //   (b) the spawns appear BEFORE the `if !config.feeds.dhan_enabled { … return }`
    //       per-feed dispatcher early-return, so they run regardless of which feed
    //       is enabled (a Groww-only run AND a Dhan-only run both spawn them).
    let bridge_pos = main
        .find("run_groww_bridge")
        .expect("main.rs must spawn run_groww_bridge");
    let lane_comment = main
        .find("Groww second feed: dormant-until-enabled")
        .expect("main.rs must carry the dormant-lane spawn comment");
    let spawn_region = &main[lane_comment..bridge_pos];
    assert!(
        !spawn_region.contains("if feeds.groww_enabled {")
            && !spawn_region.contains("if config.feeds.groww_enabled {"),
        "the Groww lane spawns MUST NOT be wrapped by a boot-time groww_enabled \
         if-block — that is the OLD model where a feed OFF-at-boot could never be \
         cold-started by the webpage toggle. Spawn the lanes dormant + self-idle."
    );
    let dhan_skip = main
        .find("if !config.feeds.dhan_enabled")
        .expect("main.rs must have the Dhan-off per-feed dispatcher skip-guard");
    assert!(
        bridge_pos < dhan_skip,
        "the Groww lanes MUST spawn BEFORE the `if !config.feeds.dhan_enabled` \
         early-return — otherwise a Dhan-OFF run would return before spawning them \
         and a Groww-only run would have no lane."
    );

    // (3) OFF-feed isolation is preserved by self-idle: the bridge AND the sidecar
    //     supervisor each gate their work on `is_enabled(Feed::Groww)`, so an OFF
    //     Groww feed touches NOTHING (no tail, no venv, no Python, no auth).
    let bridge = read_app_src("groww_bridge.rs");
    assert!(
        bridge.contains("is_enabled(Feed::Groww)"),
        "groww_bridge.rs MUST self-idle on `is_enabled(Feed::Groww)` so an OFF \
         Groww feed tails nothing (OFF-feed isolation, operator lock 2026-06-23)."
    );
    let sidecar = read_app_src("groww_sidecar_supervisor.rs");
    assert!(
        sidecar.contains("is_enabled(Feed::Groww)"),
        "groww_sidecar_supervisor.rs MUST self-idle on `is_enabled(Feed::Groww)` so \
         an OFF Groww feed provisions no venv + spawns no Python (OFF-feed isolation)."
    );

    // (4) The activation watcher reconciles on the enable flag AND owns the
    //     activation task's lifecycle so a disable cancels all in-flight Groww
    //     work (no leaked build loops, no work after OFF — hostile-review fix).
    let activation = read_app_src("groww_activation.rs");
    assert!(
        activation.contains("is_enabled(Feed::Groww)"),
        "groww_activation.rs MUST read `is_enabled(Feed::Groww)` so the lane \
         activation is driven by the live enable flag."
    );
    assert!(
        activation.contains("JoinHandle") && activation.contains(".abort()"),
        "groww_activation.rs MUST own the activation task as a JoinHandle and \
         `.abort()` it on disable — otherwise an ON→OFF→ON toggle storm leaks \
         build loops and in-flight auth/CSV work continues after OFF (OFF-feed \
         isolation, operator lock 2026-06-23)."
    );
    // (5) `running` is marked only AFTER the watch-list build succeeds — never a
    //     false-OK while activation is still in flight (operator 2026-06-24 "no
    //     illusion"). Proven positionally: the `mark_groww_lane_running()` call
    //     sits AFTER the watch-list-ready log, i.e. after the build's Ok arm.
    let mark = activation
        .find("mark_groww_lane_running")
        .expect("groww_activation.rs must mark the lane running once live");
    let watch_ready = activation
        .find("Groww watch-list ready")
        .expect("groww_activation.rs must log when the watch-list is built");
    assert!(
        watch_ready < mark,
        "groww_activation.rs MUST set `running` AFTER the watch-list is built \
         (mark_groww_lane_running must follow the build's Ok arm) — marking it \
         earlier is a false-OK: the feed page would show 'running' for an empty \
         lane (operator 2026-06-24 'no illusion')."
    );
}

#[test]
fn dhan_lanes_spawn_dormant_and_self_idle() {
    // PR-2 (feed-toggle-full-lifecycle): mirror the Groww dormant-watcher guard
    // for the Dhan lane. The Dhan activation watcher is spawned UNCONDITIONALLY
    // at boot (so a runtime toggle has a supervisor behind it) and is a
    // level-triggered, SAFETY-GATED reconciler that keeps the `dhan_lane_running`
    // UI flag honest and refuses a teardown while live trading is on.
    //
    // This guard proves the new invariant mechanically so a future edit cannot
    // (a) drop the unconditional watcher spawn, OR (b) drop the enable-flag read /
    // safety-gate / JoinHandle-free pure-reconciler shape that makes it correct.
    //
    // NOTE: PR-2 does NOT relax the existing Dhan-OFF early-return guards below —
    // the inline Dhan boot spine is still gated by `if !config.feeds.dhan_enabled`
    // (enabled-default byte-identical). The watcher spawn is added BEFORE that
    // branch, so the positional early-return assertions still hold.
    let main = read_main_rs();

    // (1) The watcher is spawned (the lane's supervisor exists for the toggle).
    assert!(
        main.contains("run_dhan_activation_watcher"),
        "main.rs MUST spawn `run_dhan_activation_watcher` so the Dhan lane has a \
         dormant supervisor behind the runtime toggle (PR-2)."
    );

    // (2) The spawn is UNCONDITIONAL, proven positionally: it appears BEFORE the
    //     `if !config.feeds.dhan_enabled` per-feed dispatcher early-return, so it
    //     runs regardless of which feed is enabled (a Dhan-OFF / Groww-only run
    //     still spawns it and it survives because that branch awaits shutdown).
    let watcher_pos = main
        .find("run_dhan_activation_watcher")
        .expect("main.rs must spawn run_dhan_activation_watcher");
    let dhan_skip = main
        .find("if !config.feeds.dhan_enabled")
        .expect("main.rs must have the Dhan-off per-feed dispatcher skip-guard");
    assert!(
        watcher_pos < dhan_skip,
        "the Dhan activation watcher MUST spawn BEFORE the \
         `if !config.feeds.dhan_enabled` early-return — otherwise a Dhan-OFF run \
         would return before spawning it and the runtime toggle would have no \
         supervisor behind it."
    );

    // (3) The watcher module drives off the LIVE enable flag AND honours the
    //     Dhan-disable safety gate (the supervisor-layer half of the two-layer
    //     gate; the handler returns CONFLICT for the other half).
    let dhan_act = read_app_src("dhan_activation.rs");
    assert!(
        dhan_act.contains("is_enabled(Feed::Dhan)"),
        "dhan_activation.rs MUST read `is_enabled(Feed::Dhan)` so the lane state is \
         driven by the live enable flag."
    );
    assert!(
        dhan_act.contains("can_disable_dhan()")
            && dhan_act.contains("reconcile_dhan_lane_action_with_gate"),
        "dhan_activation.rs MUST consult `can_disable_dhan()` via \
         `reconcile_dhan_lane_action_with_gate` so a runtime disable is REFUSED \
         while live trading is on (supervisor-layer safety gate, PR-E)."
    );

    // (4) Honest UI flag both ways — the watcher uses the two-way setter so the
    //     feed page never shows a stale "running" after a runtime teardown.
    assert!(
        dhan_act.contains("set_dhan_lane_running"),
        "dhan_activation.rs MUST use `set_dhan_lane_running` (both-ways) so the \
         feed page reports the truth across runtime toggles (no false-OK)."
    );
}

#[test]
fn both_feeds_off_is_handled_explicitly() {
    let src = read_main_rs();
    // When NEITHER feed is enabled the app must NOT silently do feed work — it
    // halts / no-feed-path. `any_enabled()` is the shared helper for that gate.
    assert!(
        src.contains("any_enabled()"),
        "main.rs MUST consult `feeds.any_enabled()` so the both-OFF case takes \
         no feed action (no auth, no instruments for either feed)."
    );
}

#[test]
fn dhan_auth_and_universe_exist_so_the_gate_actually_wraps_work() {
    // Sanity: the very work the gate protects must still be present in main.rs
    // (otherwise the gate guards nothing). If these vanish, the isolation claim
    // is vacuous and this test flags it for re-review.
    let src = read_main_rs();
    assert!(
        src.contains("authenticating with Dhan"),
        "Dhan auth step must exist in main.rs for the dhan_enabled gate to be meaningful."
    );
    assert!(
        src.contains("load_instruments") || src.contains("daily_universe"),
        "Dhan instrument load must exist in main.rs for the dhan_enabled gate to be meaningful."
    );
}

#[test]
fn dhan_off_early_returns_before_auth_and_instruments() {
    // STRENGTHENED proof (operator 2026-06-23 "strengthen it"): it is not enough
    // that the gate AND the work both exist — the OFF path must EARLY-RETURN
    // *before* Dhan auth and instrument load are ever reached. We prove this
    // positionally: the `if !config.feeds.dhan_enabled { … return … }` skip
    // block appears, and a `return` lives BETWEEN that guard and the Dhan auth
    // step. So when Dhan is OFF the function exits before touching auth /
    // instruments — they are unreachable, not merely "behind an if".
    let src = read_main_rs();

    let guard = src
        .find("if !config.feeds.dhan_enabled")
        .expect("the Dhan OFF skip-guard `if !config.feeds.dhan_enabled` must exist");
    let auth = src
        .find("authenticating with Dhan")
        .expect("the Dhan auth step must exist");
    let instruments = src
        .find("load_instruments")
        .or_else(|| src.find("load_daily_universe_plan"))
        .or_else(|| src.find("daily_universe"))
        .expect("the Dhan instrument-load step must exist");

    assert!(
        guard < auth,
        "the Dhan OFF skip-guard MUST appear before Dhan auth — so OFF never reaches auth."
    );
    assert!(
        guard < instruments,
        "the Dhan OFF skip-guard MUST appear before the Dhan instrument load — so OFF \
         never fetches/builds instruments."
    );
    // The OFF block must EARLY-RETURN before auth: a `return` must sit between
    // the guard and the auth step. Without it, OFF would fall through to auth.
    let between_guard_and_auth = &src[guard..auth];
    assert!(
        between_guard_and_auth.contains("return Ok(())"),
        "the `if !config.feeds.dhan_enabled` block MUST early-return (return Ok(())) \
         before Dhan auth — proving OFF runs ZERO auth + instrument work (operator \
         lock 2026-06-23: OFF feed = entire architecture untouched)."
    );
}
