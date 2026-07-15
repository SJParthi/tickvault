# Implementation Plan: Retire the Groww LIVE feed — REST-only runtime for both brokers

**Status:** APPROVED
**Date:** 2026-07-15
**Approved by:** Parthiban (operator, in-session, 2026-07-15). Verbatim quotes (typos preserved):
> "remove the whole Groww live feed; keep only spot 1m and option chain for both brokers; go."
> "go aehad approv ed dude"

**Base:** origin/main `699be32`. Branch: `claude/groww-live-off-rest-only`.
**Verified deletion map:** the pre-implementation verification stage produced a line-level
map (scratchpad `groww-live-delete/verification.md`); every file:line below was verified
against `699be32`.

**Crates changed:** `tickvault-common` (config.rs, constants.rs), `tickvault-core`
(feed/groww/* split + relocation, pipeline/feed_lag_monitor.rs, pipeline/feed_presence.rs,
pipeline/feed_consumer.rs, instance_lock.rs), `tickvault-app` (main.rs, groww_* modules,
tf_consistency_boot.rs, feed_scoreboard_boot.rs, groww_spot_1m_boot.rs,
spot_1m_rest_boot.rs, new groww_universe_boot.rs), `tickvault-storage` (groww/feed-gap/scale
persistence deletes, partition_manager retention rows, guard tests), `tickvault-api`
(/feeds groww toggle 409). Plus: `scripts/groww-sidecar/` (deleted), `deploy/aws/terraform/*`
(TRAP A lockstep), `deploy/aws/cloudwatch-agent.json` + `user-data.sh.tftpl`,
`config/base.toml`, `.claude/rules/project/*` retirement banners, `CLAUDE.md`.

**Seam agreements:** (a) `#1575` (C4 variant sweep) owns `error_code.rs` /
`error_code_rule_file_crossref.rs` / `error_code_tag_guard.rs` / `token_manager.rs` /
`slo_score.rs` + the wave-2 / wave-3-d / dhan-rest-canary / dhan-rest-only-noise-lock rule
files — this PR does NOT touch them; ALL ErrorCode VARIANTS STAY (banners-only split:
emit sites + dead terraform filter entries deleted; rule files gain retirement banners
saying the variants are RETAINED until the post-C4 sweep). (b) `#1549` (Groww order-side)
— zero code-file overlap. (c) Candle/TF machinery stays DORMANT this PR; a committed
follow-up retires it.

## Plan Items

- [x] Item 1 — Plan file (this file)
  - Files: .claude/plans/active-plan-groww-live-off.md
  - Tests: n/a (plan-gate satisfaction)

- [x] Item 2 — Rule-file retirement banners + dated edits (groww-scale-aws-lockout FIRST —
  its §5 forbids guard removal without a dated quote recorded there)
  - Files: .claude/rules/project/groww-scale-aws-lockout-2026-07-06.md,
    groww-second-feed-scope-2026-06-19.md, websocket-connection-scope-lock.md,
    no-rest-except-live-feed-2026-06-27.md, feed-stall-watchdog-error-codes.md,
    groww-native-rust-error-codes.md, groww-scale-error-codes.md,
    feed-gap-error-codes.md, dual-feed-scoreboard-error-codes.md,
    observability-architecture.md, operator-charter-forever.md,
    brutex-crossverify-error-codes.md, aws-budget.md (cost note lands with TRAP A)
    (CLAUDE.md: verified no-op — tripwire clean, no edit needed; de-listed fix-round-1b)
  - Tests: n/a (docs; crossref forward direction satisfied by banner mentions)

- [x] Item 3 — Seam relocations + [groww_universe] rider
  - Files: crates/core/src/feed/groww/watch_reader.rs (relocated from native/),
    crates/core/src/feed/groww/mod.rs, crates/app/src/groww_spot_1m_boot.rs (imports),
    crates/app/src/groww_universe.rs (NEW — daily watch build + master persist +
    IST midnight+120s day-roll; GROWW-MASTER-01 string stage="watch_build"; supervised
    respawn wrapper added fix-round-1b), crates/app/src/groww_watch_paths.rs (NEW —
    watch_file_path_for + secs_until_day_roll relocated),
    crates/app/tests/groww_universe_wiring_guard.rs (NEW, fix-round-1b),
    crates/common/src/config.rs ([groww_universe] serde default OFF), config/base.toml
  - Tests: watch_reader unit tests move intact; watch_file_path_for/secs_until_day_roll
    unit tests move with the fns; groww_universe day-roll math unit tests

- [x] Item 4 — The deletion (live stack)
  - Files: scripts/groww-sidecar/ (whole dir), crates/app/src/{groww_bridge.rs,
    groww_sidecar_supervisor.rs, groww_scale_ladder.rs, groww_activation.rs,
    groww_fleet_alerts.rs, groww_scale_lock.rs, groww_native_shadow.rs},
    crates/core/src/feed/groww/{native/, nats.rs, nkey.rs, proto.rs, subjects.rs,
    shard_cutter.rs, auth.rs}, crates/storage/src/{feed_gap_audit_persistence.rs,
    groww_scale_audit_persistence.rs, groww_candle_persistence.rs,
    groww_persistence.rs (partial)}, crates/app/src/main.rs (spawn sites),
    crates/core/src/pipeline/feed_lag_monitor.rs (groww half)
    (feed_consumer.rs KEPT — shared core, Dhan tick_processor delegates to it;
    feed_presence.rs UNTOUCHED — feed-generic, live consumers remain),
    crates/core/src/instance_lock.rs (GrowwScale05 emit
    arms ONLY — the named-lock knob is the GDF seam and STAYS), guard tests per the
    verified map, storage partition_manager retention rows
  - Tests: workspace green after delete; guard retunes in Item 8

- [x] Item 5 — TRAP A lockstep (one commit)
  - Files: crates/app/src/groww_spot_1m_boot.rs + crates/app/src/spot_1m_rest_boot.rs
    (tv_rest_1m_fire_heartbeat gauge, set once per fire, NOT pre-registered),
    deploy/aws/terraform/market-hours-liveness-alarm.tf (metric_name only),
    deploy/aws/terraform/{feed-stall-restart-alarm.tf DELETE, silent-feed-alarms.tf S4
    groww-lag alarm DELETE, error-code-alarms.tf feed-stall-01 entry DELETE,
    app-alarms.tf groww_ws_inactive/stall_storm DELETE, window-gate Lambda ALARM_NAMES
    6→5}, deploy/aws/cloudwatch-agent.json + deploy/aws/user-data.sh.tftpl (EMF −4
    Groww-live +1 new, identical in both), guards: cw_agent_selector_lockstep_guard,
    cloudwatch_app_alarms_wiring, error_code_paging_filter_drift_guard,
    .claude/rules/project/aws-budget.md (dated cost note)
  - Tests: the three retuned guards green

- [x] Item 6 — TRAP B (tf_consistency summary gating)
  - Files: crates/app/src/tf_consistency_boot.rs (pure should_notify_summary predicate:
    NoData → log-only; MismatchFound/Degraded/Blind stay Telegram-loud per Rule 11)
  - Tests: unit tests covering all RunStatus arms

- [x] Item 7 — events.rs + config + /feeds 409
  - Files: crates/core/src/notification/events.rs (delete variants ONLY where ALL
    constructors die — re-verified zero constructors first), crates/common/src/config.rs
    (GrowwScaleConfig + sidecar/bridge/shadow/gap keys removed; feeds.groww_enabled KEPT),
    crates/api/src (POST /api/feeds/groww → 409 "live feed retired")
  - Tests: config default tests; API 409 test

- [x] Item 8 — Guard retunes + gates
  - Files: remaining guard tests (groww_no_mint_guard partial, ws_audit_loud_drops_guard
    arms, chaos arms), quality baselines (test-count ratchet documented override,
    #1522/#1569 precedent)
  - Tests: cargo fmt --check, clippy -D warnings -W clippy::perf, cargo test --workspace
    (real counts in PR body)

## Design

The runtime becomes REST-only for BOTH brokers. The entire Groww LIVE capture stack —
Python sidecar, NDJSON bridge, stall-watchdog supervisor, scale ladder/lock/fleet, native
NATS-over-WS client + shadow, gap-episode tracker — is deleted at the module level (the
strongest guard, per the AWS-lifecycle precedent). What SURVIVES is the per-minute REST
surface (Dhan + Groww spot-1m, option-chain, the config-OFF contract-1m leg), the shared
tables with feed-in-key DEDUP, the token-minter consumer-only SSM read, the #1572
dhan_intraday_parse float fix, the GDF pluggable seam (FeedsConfig + the named-lock knob in
instance_lock.rs), and the brutex §37 comparison machinery (dormancy-noted).

Two structural seams make the deletion safe rather than destructive:

1. **watch_reader relocation** — `parse_watch_file`/`WatchFileDoc` move from the deleted
   `native/` tree into the kept `core/feed/groww/watch_reader.rs`; `watch_file_path_for` +
   `secs_until_day_roll` re-home there from the deleted `groww_native_shadow.rs`. The KEEP
   spot leg (`groww_spot_1m_boot.rs` VIX resolution) imports from the new home BEFORE the
   old homes are deleted.
2. **[groww_universe] rider** — the daily watch-set build + `persist_groww_instruments`
   previously lived inside the deleted activation watcher. A new process-global daily task
   (`groww_universe_boot.rs`, `[groww_universe]` config, serde default OFF, base.toml opts
   in) re-homes: master CSV download → `build_and_write_groww_watch` →
   `persist_groww_instruments` (GROWW-MASTER-01 gains string stage="watch_build") → IST
   midnight+120s day-roll. Without it the VIX spot resolution and the SEBI lifecycle
   master's `feed='groww'` rows would silently die.

TRAP A: the CloudWatch alarm on `tv_groww_exchange_lag_p99_seconds` has
`treat_missing_data="breaching"` and its SOLE producer dies with the bridge → daily false
SOS. Fix: a new `tv_rest_1m_fire_heartbeat` gauge set once per fire by BOTH spot legs
(deliberately NOT pre-registered — presence IS the liveness signal); the alarm edit is
metric_name only; EMF selectors, the window-gate Lambda list, and the dead alarms move in
the SAME commit (lockstep guards force this).

TRAP B: the 15:40 IST TF-consistency summary would page NoData daily once Groww candles
stop. A pure `should_notify_summary` predicate gates the Telegram: NoData → log-only;
MismatchFound / Degraded / Blind stay loud (Rule 11 — blind is never quiet).

Order/position live channels are OUT of scope (operator 2026-07-15: market data = per-minute REST pull; order/position events = live push, always — `order_update_connection.rs` untouched).

## Edge Cases

- **VIX watch-file dependency:** `try_resolve_vix_target` reads the daily watch file. With
  `[groww_universe]` OFF (serde default) and base.toml ON, a config that omits the section
  leaves VIX unresolved — the existing fail-soft arm (`stage="vix_unresolved"`, counted,
  warned once) already covers this; core 3 indices unaffected.
- **Day-roll across IST midnight:** the rider re-derives the date per attempt
  (`is_valid_trading_date` guard) and sleeps `secs_until_day_roll` (midnight+120s grace) —
  a suspend crossing midnight re-derives on wake; never a stale-date watch file.
- **Watch build on holidays/weekends:** `build_and_write_groww_watch` retry loop keeps the
  bounded `min(10 * 2^n, 300)`s backoff; a failed build never blocks boot (the rider is a
  spawned task, not a boot gate).
- **feeds.groww_enabled semantics shift:** it now gates REST-leg feed identity + the
  [groww_universe] task only. The /feeds groww toggle returns 409 — a runtime "enable" of a
  deleted live feed must refuse loudly, never silently no-op.
- **Heartbeat gauge on a REST-off day:** both spot legs OFF ⇒ no heartbeat samples ⇒ the
  liveness alarm's missing-data=breaching fires — correct: a runtime with zero REST fires
  in market hours IS dead. Window-gate arming (09:20–15:35 IST) bounds false pages.
- **groww_persistence.rs partial delete:** tick_row_builder + shadow_candle_writer
  references survive (dormant candle machinery) — only the live-ingest half dies.
- **instance_lock.rs:** ONLY the three GrowwScale05 emit arms die; the named-lock knob
  (compute_named_lock_path / try_acquire_named_lock / spawn_named_lock_heartbeat) is the
  GDF seam and must survive byte-identical.
- **ErrorCode variants:** every variant stays (crossref forward direction satisfied by the
  retirement banners; the paging drift guard is satisfied by deleting the feed-stall-01 tf
  entry in the same PR as its emit sites).

## Failure Modes

- **Missed spawn-site edit in main.rs** → compile error (deleted modules) — fail-closed at
  build time, not runtime.
- **Import fixed after delete instead of before** → transient broken tree inside the PR;
  mitigated by the mandated commit order (relocation commit precedes deletion commit).
- **TRAP A half-shipped** (alarm re-pointed but EMF selector not shipped, or vice versa) →
  the cw_agent_selector_lockstep_guard + cloudwatch_app_alarms_wiring guards fail the
  build; the lockstep is mechanical, not procedural.
- **TRAP B regression** (predicate inverted) → unit tests pin all four RunStatus arms.
- **Rider dies at runtime** → the task is spawned supervised-style with bounded backoff;
  a dead rider means tomorrow's watch file is missing → VIX unresolved (fail-soft, counted)
  + no new lifecycle rows (idempotent next-boot re-run heals; DEDUP UPSERT keys).
- **Test-count ratchet trips on deletion** → documented override per the #1522/#1569
  precedent (before/after counts in the PR body); NEVER --no-verify.
- **Plan-count cap (V7, max 5):** adding this file makes 6 locally; the merge-train driver
  is sweeping main — recount at push time and report if >5 (per the work order).

## Test Plan

- `cargo fmt --check` clean.
- `cargo clippy --workspace -- -D warnings -W clippy::perf` clean.
- `cargo test --workspace` green (config.rs touched ⇒ workspace scope per testing-scope.md);
  real pass counts pasted in the PR body.
- New unit tests: groww_universe day-roll math + config default OFF;
  should_notify_summary all arms; relocated watch_reader/watch_file_path_for/
  secs_until_day_roll tests move intact; /feeds groww 409 test.
- Retuned guards green: cw_agent_selector_lockstep_guard, cloudwatch_app_alarms_wiring,
  error_code_paging_filter_drift_guard, groww_no_mint_guard (surviving arms :54/:89/:215).
- Test-count ratchet: before/after counts recorded; documented override.

## Rollback

Single-PR revert: `git revert` of the squash-merge commit restores the entire live stack
(module-level deletes are cleanly revertible; no data migration occurs — tables are never
dropped, rows never deleted; the `feed='groww'` live rows simply stop being written).
Config rollback alone is NOT sufficient (code is deleted) — rollback is the revert.
The [groww_universe] rider is independently disable-able via config (serde default OFF)
without reverting. Terraform: the TRAP A commit is self-contained; reverting it restores
the old alarms/selectors in lockstep.

## Observability

- NEW gauge `tv_rest_1m_fire_heartbeat` (both spot legs, once per fire, NOT
  pre-registered) — the market-hours liveness alarm's new subject (TRAP A).
- GROWW-MASTER-01 gains string stage="watch_build" (no new ErrorCode) — the rider's
  watch-build failure surface; existing stages unchanged.
- Retired paging: FEED-STALL-01 filter+alarm, groww-lag S4 alarm, groww_ws_inactive /
  stall_storm alarms, feed-stall-restart counter alarm — all deleted WITH their producers
  (no dead monitors); observability-architecture.md "Retired paging entries" updated.
- TF-consistency summary: NoData → log-only (`info!`); MismatchFound/Degraded/Blind stay
  Telegram-loud; all counters unchanged.
- The REST legs' existing edge-paging (SPOT1M-01/CHAIN-02 escalation, CloudWatch backstops)
  is UNTOUCHED and is the operator's primary signal for the surviving surface.
- Scoreboard: episode aggregation + REST digest survive; only the feed_gap dangling-close
  sweep step (6f) is removed with its table.

## Per-Item Guarantee Matrix

The 15-row "100% everything" matrix and the 7-row Resilience matrix apply per
`.claude/rules/project/per-wave-guarantee-matrix.md` (explicit cross-reference — every row
binds this plan; deletions carry ratchet-guard retunes in the same PR, no new hot-path
code is introduced, no new tick-drop path exists because the live tick path itself is
retired by operator directive). Honest 100% claim (mandatory wording): 100% inside the
tested envelope, with ratcheted regression coverage: the surviving REST legs keep their
bounded fetch ladders, persist-gated escalation edges and DEDUP-idempotent re-appends;
the deletion is module-level and single-revert recoverable. NOT claimed: any Groww live
tick capture — by operator directive 2026-07-15 there is NONE; Groww market data is the
per-minute official REST candles + chain only, so intraminute Groww price movement is
invisible between fetches.

## Committed Follow-ups (fix-round-1b, 2026-07-15 — NOT in this PR)

- **C-phase candle/TF retirement:** the Groww candle/TF seal machinery stays DORMANT in
  this PR (the seal_routing Groww emit site, the seals-emitted dashboard widget, the EMF
  seal rows in cloudwatch-agent.json / user-data.sh.tftpl, and the telegram-webhook
  friendly-name map entry are all deliberately KEPT dormant); a dedicated C-phase PR owns
  their full retirement. `ensure_named_views` is also dormant (zero call sites).
- **Contract-1m operator kill-decision PENDING:** the Groww per-contract 1m REST leg
  (§38 PR-4) remains enabled-capable; whether it survives without the live chain anchor
  cadence is an operator decision — do not delete or degrade it without a dated quote.
- **GDF-trial operator decision PENDING:** the GDF feed #3 trial
  (`gdf-third-feed-scope-2026-07-13.md`) awaits its own operator go; NOTE the Q1 tension —
  the 2026-07-15 REST-only directive ("keep only spot 1m and option chain for both
  brokers") vs the GDF lock's live-WS trial design needs an explicit operator ruling
  before PR-B starts.
- **Presence-registry producer-less dormancy:** `feed_presence.rs` is retained
  feed-generic but its Groww producer is gone — the registry runs producer-less for the
  Groww slots (scoreboard coverage columns read the SQL fallback); a future feed re-wire
  or C-phase cleanup decides its fate.
