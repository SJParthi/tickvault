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
fn off_feed_reconciler_never_emits_start() {
    // PR-4 strengthening of the #1192 "OFF feed = nothing touched" invariant from a
    // SOURCE-SCAN of the spawn shape to a BEHAVIOURAL proof of the dormancy decision:
    // an OFF feed's pure reconciler must NEVER emit `Start` (the only action that
    // triggers cold-start auth/instrument/connect work), in ANY activated state it
    // could legally reach while OFF. So the dormant watcher does ZERO start work
    // while the feed is OFF — the stronger, true invariant (operator lock 2026-06-23).
    // The exhaustive sequence storms live in `feed_toggle_lifecycle_guard.rs`; this
    // sits beside the spawn-shape guards so the #1192 file alone pins both halves.
    use tickvault_app::{dhan_activation, groww_activation};

    for &activated in &[false, true] {
        assert_ne!(
            groww_activation::reconcile_lane_action(false, activated),
            groww_activation::LaneAction::Start,
            "OFF Groww feed must never reconcile to Start (activated={activated})"
        );
        // Dhan via the safety-gated reconciler, across both gate states — a disable
        // may yield None (gate closed) or Stop (gate open) but never Start.
        for &disable_allowed in &[false, true] {
            assert_ne!(
                dhan_activation::reconcile_dhan_lane_action_with_gate(
                    false,
                    activated,
                    disable_allowed
                ),
                dhan_activation::LaneAction::Start,
                "OFF Dhan feed must never reconcile to Start \
                 (activated={activated}, gate={disable_allowed})"
            );
        }
    }
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
fn dhan_off_returns_before_auth_and_instruments() {
    // STRENGTHENED proof (operator 2026-06-23 "strengthen it"): it is not enough
    // that the gate AND the work both exist — the OFF path must RETURN *before*
    // Dhan auth and instrument load are ever reached. We prove this positionally:
    // the `if !config.feeds.dhan_enabled { … return … }` skip block appears, and a
    // `return` lives BETWEEN that guard and the Dhan auth step. So when Dhan is OFF
    // the function exits before touching auth / instruments — they are unreachable,
    // not merely "behind an if".
    //
    // D2-pre (behaviour-identical hoist, 2026-06-26): the OFF block now RETURNS the
    // PROCESS-shared-infra runtime (`return run_shared_infra_only(...).await;`)
    // instead of a bare `return Ok(())`. It still returns BEFORE auth/instruments
    // (proven below) and `run_shared_infra_only` itself does NO Dhan auth, NO
    // instrument fetch, NO Dhan WebSocket — so the OFF-isolation guarantee holds.
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
    // The OFF block must RETURN before auth: a `return` must sit between the guard
    // and the auth step. Without it, OFF would fall through to auth.
    let between_guard_and_auth = &src[guard..auth];
    assert!(
        between_guard_and_auth.contains("return run_shared_infra_only(")
            || between_guard_and_auth.contains("return Ok(())"),
        "the `if !config.feeds.dhan_enabled` block MUST return (the shared-infra-only \
         runtime, or Ok(())) before Dhan auth — proving OFF runs ZERO auth + instrument \
         work (operator lock 2026-06-23: OFF feed = entire architecture untouched)."
    );

    // D2-pre: the shared-infra function the OFF block returns must NOT itself reach
    // Dhan auth or instrument load — the OFF-isolation guarantee survives the hoist.
    let shared = src
        .find("async fn run_shared_infra_only(")
        .expect("run_shared_infra_only must exist (D2-pre shared-infra hoist)");
    let shared_body_end = src[shared..]
        .find("\nasync fn run_shutdown_fast(")
        .map(|rel| shared + rel)
        .unwrap_or(src.len());
    let shared_body = &src[shared..shared_body_end];
    assert!(
        !shared_body.contains("authenticating with Dhan")
            && !shared_body.contains("load_instruments")
            && !shared_body.contains("TokenManager::initialize")
            && !shared_body.contains("create_websocket_pool"),
        "run_shared_infra_only MUST NOT authenticate, fetch instruments, or spawn a \
         Dhan WebSocket — it brings up ONLY the PROCESS-shared infra (API server, \
         seal-writer, aggregator, run-loop). The OFF-feed-isolation guarantee + the \
         2-WS Dhan lock both hold."
    );
}

#[test]
fn api_server_up_in_dhan_off_mode() {
    // D2-pre (C1 fix, 2026-06-26): a Dhan-OFF boot must STILL bring up the HTTP
    // API server (so the `/api/feeds` toggle endpoint is reachable) + the candle
    // seal-writer (so Groww candles seal — C2) + the main run-loop. Before the
    // hoist, all of that spawned ONLY inside the Dhan block, so a Dhan-OFF boot
    // had no API server and `/api/feeds` did not exist.
    //
    // SOURCE-SCAN ratchet: the Dhan-OFF branch must return a function that builds
    // the API server (axum::serve), the seal-writer, the aggregator, and the
    // run-loop — proving the shared infra is up on the Dhan-OFF path.
    let src = read_main_rs();

    // (1) The OFF branch routes to the shared-infra runtime (not a bare return).
    let guard = src
        .find("if !config.feeds.dhan_enabled")
        .expect("the Dhan OFF skip-guard must exist");
    let after_guard = &src[guard..];
    assert!(
        after_guard
            .find("return run_shared_infra_only(")
            .map(|pos| {
                // The route must be the FIRST thing the OFF block does after its
                // logging — well before any fall-through.
                pos < after_guard
                    .find("let fast_cache")
                    .unwrap_or(after_guard.len())
            })
            .unwrap_or(false),
        "the `if !config.feeds.dhan_enabled` block MUST `return run_shared_infra_only(...)` \
         so a Dhan-OFF boot brings up the PROCESS-shared infra (API server + seal-writer \
         + aggregator + run-loop) instead of bare-returning."
    );

    // (2) The shared-infra function must actually spawn the API server, the
    // seal-writer, the aggregator, and the run-loop.
    let shared = src
        .find("async fn run_shared_infra_only(")
        .expect("run_shared_infra_only must exist (D2-pre)");
    let shared_body_end = src[shared..]
        .find("\nasync fn run_shutdown_fast(")
        .map(|rel| shared + rel)
        .unwrap_or(src.len());
    let body = &src[shared..shared_body_end];
    for (needle, what) in [
        ("axum::serve", "the HTTP API server"),
        ("build_router_with_auth", "the /api/feeds toggle routes"),
        (
            "spawn_seal_writer_loop",
            "the candle seal-writer (Groww candles seal — C2)",
        ),
        ("spawn_engine_b_aggregator", "the 21-TF aggregator"),
        ("wait_for_shutdown_signal", "the main run-loop"),
    ] {
        assert!(
            body.contains(needle),
            "run_shared_infra_only MUST bring up {what} (`{needle}`) so a Dhan-OFF boot \
             has the PROCESS-shared infra running (C1/C2 fix)."
        );
    }
}
