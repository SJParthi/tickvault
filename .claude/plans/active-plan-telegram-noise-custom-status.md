# Implementation Plan: Telegram Noise Cut PR-1 — CustomStatus variant + commandment reword (F2+F4)

**Status:** APPROVED
**Date:** 2026-07-10
**Approved by:** Parthiban (operator) — standing directive 2026-07-10, via coordinator

## Plan Items

- [ ] Add `NotificationEvent::CustomStatus { message }` at `Severity::Low`; keep `Custom` at `Severity::High`
  - Files: crates/core/src/notification/events.rs
  - Tests: test_custom_status_severity_is_low, test_custom_is_high, test_custom_status_episode_key_is_none, test_low_custom_status_never_immediate_or_sms
- [ ] Flip the 7 informational Custom emit sites to CustomStatus (Low); keep the 4 actionable at Custom (High)
  - Files: crates/app/src/main.rs, crates/app/src/orphan_position_watchdog_boot.rs
  - Tests: telegram_custom_noise_guard.rs (status_pings_use_custom_status_not_custom, actionable_custom_sites_stay_custom_high)
- [ ] Reword the jargon-carrying Custom/CustomStatus bodies for commandments #2/#10
  - Files: crates/app/src/main.rs
  - Tests: telegram_custom_noise_guard.rs (custom_bodies_carry_no_impl_jargon, custom_bodies_have_no_redundant_severity_prefix)

## Design

The `Custom` variant is hardcoded `Severity::High`, so every informational status
ping ("Dhan feed started/stopped", "FAST BOOT", "Market closed", "Recovered
prices", "backups growing", "service auto-restarted") fires an immediate Telegram
**+ SNS SMS** (`service.rs:501` gates SMS on `severity >= High`) carrying no
operator action. This PR introduces ONE new typed variant
`CustomStatus { message: String }` at `Severity::Low` (informational,
non-actionable). Genuine alerts stay on `Custom { message }` at `Severity::High`.

A NEW VARIANT (not a `severity` field on `Custom`) is chosen deliberately: it
leaves every existing `Custom { message }` construction — including 9 test-only
literals across 3 crates — byte-for-byte untouched, so the blast radius is
exactly 3 exhaustive match arms in `events.rs` (`message_body`, `topic`,
`severity`). All other accessors (`episode_key`, `boot_milestone`,
`episode_role`, `dispatch_policy`, `feed_badge`) have `_` wildcards, so
`CustomStatus.episode_key() == None` (NOT episode-routed — episode machinery is
untouched, F1 deferred).

7 informational emit sites are flipped to `CustomStatus`; the 4 genuinely
actionable sites (QuestDB unavailable at boot, low disk space, and the two 15:25
orphan-position diagnostics) stay `Custom`/High. The same diff rewords the
impl-jargon bodies (commandment #2: "ring buffer", "disk spill", "spill files",
"BUFFERING mode", "docker compose up -d", "Docker container", "QuestDB",
"WebSockets") and removes redundant in-body `CRITICAL:`/`WARNING:` prefixes
(commandment #10 — dispatch already prepends the severity emoji at
`service.rs:448`).

NOT touched: `episode.rs`, `coalescer.rs`, `service.rs` classify/dispatch logic
(beyond a new variant's exhaustive-match requirement — there is none outside
`events.rs`); the runtime-incident bubble (F1); boot-Custom fold (F3); auth
convergence (F5); recovery-ping demotion (F6). No `crates/common` change (avoids
workspace-wide test escalation). No §28 indicator/strategy change. No WS
endpoint.

## Edge Cases

- `Info < Low < Medium < High < Critical` (Severity is `Ord`): `CustomStatus`
  at `Low` is strictly `< High`, so it can never trigger SMS.
- `CustomStatus` and `Custom` have distinct `topic()` strings
  (`"CustomStatus"` vs `"Custom"`), so demoted status pings coalesce in their
  own bucket and never merge with a genuine High `Custom` alert.
- `CustomStatus.dispatch_policy() == Default` (wildcard) → routed by severity:
  Low → Digest (in-market) / Coalesce60 (off-hours), never Immediate.
- The 8905 spill-size ping demotes to `CustomStatus` because the root cause
  (`QuestDbDisconnected`, Critical) already pages in the same watchdog loop
  (`main.rs:8925`) — demoting avoids a duplicate page, not a lost alert.
- Ambiguous-actionability sites fail safe toward KEEPING `Custom`/High.

## Failure Modes

- Over-demotion of a real alert → `actionable_custom_sites_stay_custom_high`
  pins the 4 High sites (2 in main.rs, 2 in orphan boot).
- A status ping regressing back to High `Custom` →
  `status_pings_use_custom_status_not_custom` fails the build.
- Re-introduced impl jargon in any Custom/CustomStatus body →
  `custom_bodies_carry_no_impl_jargon` bans the substrings.
- Re-introduced redundant severity prefix →
  `custom_bodies_have_no_redundant_severity_prefix`.
- Accidental episode coupling of `CustomStatus` →
  `test_custom_status_episode_key_is_none`.

## Test Plan

- Scoped per `testing-scope.md`: `cargo test -p tickvault-core -p tickvault-app`
  (core + app only; NOT `--workspace` — no `common` change so no escalation).
- New unit tests in `events.rs`: severity pin, episode-key None, dispatch-lane
  non-immediate/no-SMS (via pure `classify_dispatch`).
- New source-scan guard `crates/app/tests/telegram_custom_noise_guard.rs`:
  status-routing lane, actionable-stays-High lane, jargon ban, redundant-prefix
  ban. The scanner strips its own scope to Custom/CustomStatus message literals
  (constructor-anchored) so it never matches its own assertion literals, log
  lines, or comments (self-test-safe).
- `cargo fmt --check`, `cargo clippy -p tickvault-core -p tickvault-app`,
  banned-pattern scanner, pub-fn guards, plan-verify.

## Rollback

Purely additive: a new enum variant + reclassification of 7 call sites + body
rewording. No schema, no config, no persisted state, no CloudWatch alarm change
(alarms key off `error!` ErrorCodes, never `Custom` severity — `Custom` emits
no ErrorCode). Reverting the single squash commit restores byte-identical prior
behaviour.

## Observability

No new metric/counter/alarm. The existing `tv_telegram_dispatched_total{severity}`
label shifts `high` → `low` for the 7 flipped sites and those sites stop firing
the SNS SMS leg (the intended noise cut). No CloudWatch alarm reads `Custom`
severity, so there is zero alerting regression — verified: none of the alarmed
`error!` ErrorCodes originate from a `Custom` emit site.

## Zero-Loss Guarantee Charter check

- Coverage / audit / logging / alerting: N/A — notification-severity only; no
  tick / persist / order / hot path touched (the QuestDB-unavailable alert body
  is reworded but the alert STILL fires at High).
- No new tick-drop path; no hot-path allocation (`CustomStatus` is a cold-path
  notify construction; `severity()` is a `Copy` read; the DHAT dispatcher pin on
  the episode bypass arm is unaffected — `CustomStatus.episode_key() == None`).
- 3-agent adversarial review (hot-path + security + hostile) recommended by
  charter §E is delegated to the coordinator's review gate after this PR opens.

## Per-item guarantee matrix

This item cross-references `.claude/rules/project/per-wave-guarantee-matrix.md`
for the full 15-row "100% everything" matrix + 7-row resilience matrix. Rows
that apply mechanically to this notification-severity PR: code coverage (new
ratchets), code checks (banned-pattern + pub-fn + plan-verify), monitoring
(existing `tv_telegram_dispatched_total`), logging (unchanged `error!`/`warn!`
sites), functionalities (every new item has a call site + test), extreme check
(build-failing ratchets R1–R7). Rows N/A (tick/persist/order path, DEDUP keys,
DHAT hot-path budgets, chaos scenarios) are marked N/A because this PR touches
only the Telegram notification severity/body surface.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Dhan lane toggled ON off-hours | "Dhan feed started" CustomStatus (Low), batched, NO SMS |
| 2 | QuestDB unavailable at boot | High "Price database unavailable" immediate + SMS (kept) |
| 3 | Post-market close on a trading day | "Market closed" CustomStatus (Low), batched, no jargon, no SMS |
| 4 | Docker container unhealthy | CustomStatus auto-restart ping, no SMS, no "docker compose" / "Docker container" text |
| 5 | 15:25 orphan-position check fails | Custom (High) 🆘 immediate + SMS (kept) |
| 6 | Low disk space | Custom (High) immediate + SMS (kept), no "spill files" text |
