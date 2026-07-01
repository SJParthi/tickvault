# Implementation Plan: Gate the boot docker-compose-up CRITICAL on QuestDB health + drop a useless u64 conversion (consolidated boot/CI hardening)

**Status:** APPROVED
**Date:** 2026-07-01
**Approved by:** Parthiban (operator) — false-CRITICAL hardening surfaced by the 2026-07-01
prod boot re-check (`scratchpad/verify-boot.md` GAP-B: 11:09 IST crash-recovery boot fired
`docker compose up failed after 5 attempts → Telegram CRITICAL` while QuestDB was already up
and the app ran healthily). Standing approval for the alerting-hole hardening queue. This
single consolidated PR combines that `tickvault-app` boot fix with a trivial `tickvault-core`
clippy cleanup (drop a useless `u64::from` on an already-`u64` `security_id`) per the operator's
fewer-bigger-PRs preference.

## Included change 2 (trivial): drop useless `u64::from(security_id)` in `tickvault-core`

`crates/core/src/pipeline/tick_processor.rs::TickDedupRing` computes the FNV-1a dedup hash.
After the `security_id` u32→u64 widening, `security_id` is already `u64`, so `h ^= u64::from(security_id)`
is a useless same-type conversion (clippy `useless_conversion`). Changed to `h ^= security_id`.
NO logic change — the hash value, the dedup key, and every downstream behaviour are byte-identical;
`u64::from(x: u64)` is the identity. This clears a clippy warning that would otherwise fail the
`clippy -D warnings` CI gate. Rollback = revert the one-line hunk. No new tests needed (the existing
`TickDedupRing` dedup tests already exercise the hash; the change is provably behaviour-preserving).

## Design

`crates/app/src/infra.rs::ensure_infra_running` runs `docker compose up -d --force-recreate`
up to `COMPOSE_UP_MAX_RETRIES = 5` (infra.rs:191-215). `--force-recreate` (infra.rs:1160) is
NOT idempotent — it tears down + recreates every container on every boot, so on a same-day
crash-recovery boot (QuestDB already up + busy) the recreate can fail all 5 attempts. On
failure the code unconditionally fires `tracing::error!("... Telegram CRITICAL alert should
fire.")` (infra.rs:216-221) and `return`s BEFORE the `wait_for_service_healthy` probe
(infra.rs:225) — so the CRITICAL never consults whether the service the app actually needs
(QuestDB) is reachable. Evidence (`verify-boot.md`): the 11:09 boot survived and ran healthily
(`QuestDB ready — boot probe succeeded`, ticks flowing, tick conservation OK) precisely because
QuestDB was already up. Paging CRITICAL when the required service is demonstrably healthy is a
false-CRITICAL that erodes operator trust in the pager.

**Fix (scoped):** when compose-up exhausts its retries, probe the required service (QuestDB)
via the SAME `is_service_reachable(host, http_port)` used by `wait_for_service_healthy`. Route
the outcome through a pure, unit-testable decision function
`classify_compose_outcome(compose_succeeded, required_service_reachable) -> ComposeOutcome`:

| compose_succeeded | required_service_reachable | ComposeOutcome | Log level |
|---|---|---|---|
| true  | (n/a) | `Succeeded`          | info  |
| false | true  | `DegradedServiceUp` | warn (Low — NOT CRITICAL, no page) |
| false | false | `Critical`          | error (CRITICAL — real outage, keeps paging) |

Only the genuine `compose down + service actually unreachable` case keeps the CRITICAL. The
false-CRITICAL (`--force-recreate` churn while QuestDB is up) becomes a Low `warn!` that names
the reason. This does NOT suppress a real compose failure where the service is down.

## Edge Cases
- compose-up succeeds first attempt → `Succeeded`, unchanged behaviour (health probe still runs).
- compose-up fails 5× but QuestDB reachable (the observed 11:09 case) → `DegradedServiceUp`, warn, boot continues, health probe still runs.
- compose-up fails 5× AND QuestDB unreachable → `Critical`, error (page fires) — a REAL outage, preserved.
- QuestDB probe itself flaky at the decision instant → single TCP probe with the existing `INFRA_PROBE_TIMEOUT`; a false-negative here just keeps the (correct) CRITICAL — fail-safe toward paging, never toward silence.
- Docker daemon down / SSM creds missing → EARLY `return` upstream (infra.rs:120-146), never reaches this branch — untouched.

## Failure Modes
- The probe is best-effort and cannot panic (`is_service_reachable` swallows parse/connect errors → bool).
- If the probe wrongly reports QuestDB down, we keep CRITICAL (louder, safe). If it wrongly reports up, the downstream `wait_for_service_healthy` + the app's own boot QuestDB probe (BOOT-01/02) still catch a genuinely dead QuestDB — defence in depth, no silent proceed.
- No new hot-path code (boot-only, cold path). No allocation on any tick path.

## Test Plan
- Pure-function truth table `classify_compose_outcome`: 3 unit tests (`Succeeded`, `DegradedServiceUp`, `Critical`) + a `ComposeOutcome` `is_critical()`/label stability test.
- Assert the CRITICAL branch fires ONLY on `(false, false)`.
- `cargo test -p tickvault-app --lib` (touched crate = app).
- Hooks: banned-pattern, pub-fn-test, pub-fn-wiring, plan-verify.

## Rollback
- Single-file change in `crates/app/src/infra.rs` (new pure fn + rewired `!compose_succeeded` branch). Revert the commit to restore the prior unconditional-CRITICAL behaviour. No schema, no config, no data migration, no wire-format change.

## Observability
- `DegradedServiceUp` logs `warn!` (Low, no Telegram page) with a plain-English reason ("services already up despite compose recreate churn").
- `Critical` keeps the existing `tracing::error!` CRITICAL (routes to Telegram via the 5-sink pipeline) — now correctly gated on the required service being actually unreachable.
- New static-label counter `tv_boot_compose_recreate_degraded_total` incremented on the `DegradedServiceUp` path so the operator can see how often `--force-recreate` churns while the service is up (trend signal without paging).

## Plan Items
- [x] Add pure `ComposeOutcome` enum + `classify_compose_outcome(compose_succeeded, required_service_reachable)` in `crates/app/src/infra.rs`
  - Files: crates/app/src/infra.rs
  - Tests: test_classify_compose_outcome_succeeded, test_classify_compose_outcome_degraded_service_up, test_classify_compose_outcome_critical_only_when_service_down, test_compose_outcome_is_critical_stable
- [x] Rewire the `!compose_succeeded` branch (infra.rs:216-222) to probe QuestDB + route via the pure fn (warn Low vs error CRITICAL); increment `tv_boot_compose_recreate_degraded_total` on the degraded path
  - Files: crates/app/src/infra.rs
  - Tests: (covered by the pure-function truth table above)
- [x] Drop the useless `u64::from(security_id)` in the FNV-1a dedup hash (already-`u64` after the widening) — clippy `useless_conversion` cleanup, behaviour-preserving
  - Files: crates/core/src/pipeline/tick_processor.rs
  - Tests: (behaviour-preserving identity conversion; existing `TickDedupRing` dedup tests cover the hash)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | compose-up OK first try | info, health probe runs, no change |
| 2 | compose-up fails 5×, QuestDB reachable (11:09 case) | warn Low, counter++, boot continues, NO page |
| 3 | compose-up fails 5×, QuestDB unreachable | error CRITICAL, page fires (real outage) |
| 4 | `tickvault-core` dedup hash with `u64` `security_id` | identical hash; clippy clean; `cargo check -p tickvault-core` green |
