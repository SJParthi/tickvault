# Dual-Instance Lock Hardening — Always-On + Lock-Before-Mint (Operator Lock 2026-07-04)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F > this file >
> `wave-4-error-codes.md` (RESILIENCE-01 stub) > defaults.
> **Scope:** PERMANENT. Every boot path, every PR, every future Claude/Cowork session.
> **Operator-locked:** 2026-07-04 — operator approved "go" (this session) after the
> local-vs-AWS coexistence audit.
> **Companion code:** `crates/app/src/main.rs` (`start_dhan_lane` Step 6a-prime,
> `FORCE_INSTANCE_TAKEOVER_FLAG`), `crates/core/src/instance_lock.rs`
> (`try_acquire_instance_lock`, `force_takeover_instance_lock`,
> `spawn_instance_lock_heartbeat` held-flag), `crates/core/src/auth/token_manager.rs`
> (`mint_refused_by_instance_lock` tripwire),
> `crates/common/src/error_code.rs::ErrorCode::{Resilience01DualInstanceDetected,
> Resilience03MintRefusedLockNotHeld}`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires this
> file to mention every `Resilience03*` variant verbatim — `RESILIENCE-03` and
> `Resilience03MintRefusedLockNotHeld` appear below.
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The incident class this closes (coexistence audit 2026-07-04, Verified)

A default local Mac boot (`feeds.dhan_enabled=true`, env defaults to `prod`, no
local token cache) TOTP-mints a fresh Dhan JWT from the SAME `/tickvault/prod`
SSM credentials the AWS box uses. Dhan enforces **one active token at a time**
(`authentication.md` rule 5), so the local mint INVALIDATES the AWS box's JWT;
the AWS renewal then fails (`RenewToken` only renews ACTIVE tokens), falls back
to `generateAccessToken`, re-mints, kills the LOCAL token → a cross-host mint
ping-pong (each mint throttled by Dhan's ~125s cooldown). The RESILIENCE-01
dual-instance SSM lock that should prevent this had two fatal gaps:

1. **Mode-gated OFF** — `if trading_mode.is_live()` while BOTH prod and local
   run `mode = "sandbox"` → the lock NEVER ran anywhere.
2. **Ordered AFTER the mint** — Step 6a-prime ran after Step 6 auth, so the
   damage (token invalidation) preceded the check.

## §1. The rule (one line)

**Every Dhan-enabled boot (sandbox/dry-run INCLUDED) acquires the SSM
dual-instance lock BEFORE the Step 6 token mint; a losing peer halts with
RESILIENCE-01 before it can touch Dhan auth, and any mint attempted after lock
loss is refused fail-closed with RESILIENCE-03.**

## §2. The contract (mechanical)

| Element | Locked behaviour |
|---|---|
| Gating | The lock runs whenever the Dhan lane runs (`feeds.dhan_enabled == true`). NO trading-mode gate — sandbox burns the same shared JWT. |
| Ordering | Step 6a-prime (lock) runs BETWEEN Step 5.5 (IP verify) and Step 6 (auth/mint). The lock needs only an AWS SSM client — never a Dhan token. Ratchet: `secret_manager.rs::test_instance_lock_acquired_before_token_mint`. |
| AlreadyHeld (fresh peer) | HALT, RESILIENCE-01 `error!` + `DualInstanceDetected` Telegram — unchanged semantics, now pre-mint. |
| SSM transport error | Bounded 3-attempt / exponential (2s, 4s) retry — the SAME budget as the Step 6 SSM credential fetch — then HALT fail-closed (cannot prove there is no peer). This surfaces an SSM outage one step earlier than Step 6 would; it does not add a new boot-failure class. |
| Crashed holder | 90s TTL auto-clears via the stale-takeover path in `try_acquire_instance_lock` (`LockValue::is_stale`) — no flag needed. Ratchet: `instance_lock.rs::test_crashed_holder_stale_after_ttl_is_takeover_eligible`. |
| Escape hatch | `--force-instance-takeover` CLI flag → `force_takeover_instance_lock` overwrites the lock with a LOUD `error!` audit line naming the displaced holder — CloudWatch-logged; **NOT independently paged today** (2026-07-06 truth-sync: RESILIENCE-01/03 have no log-filter alarm; adding one is a single `error_code_alerts` map entry in `deploy/aws/terraform/error-code-alarms.tf` — flagged follow-up). Wedged/corrupt-lock use only; never routine. |
| Mint tripwire (RESILIENCE-03) | `TokenManager` carries `instance_lock_held: Option<Arc<AtomicBool>>`. `acquire_token` (the `generateAccessToken` MINT path only — `RenewToken` is untouched) refuses fail-closed BEFORE TOTP/network when the flag reads `false` (heartbeat lost the lock, or shutdown released it), emitting `ErrorCode::Resilience03MintRefusedLockNotHeld` + Telegram. The refusal classifies as a permanent auth error so retry loops fail fast. |
| Always-on ratchet | `secret_manager.rs::test_instance_lock_not_gated_on_live_mode` pins the `LOCK BEFORE MINT` marker + absence of the `is_live()` gate + the tripwire flag wiring. |

## §3. RESILIENCE-03 — token mint refused, instance lock not held

**Severity:** Critical. **Auto-triage safe:** No (Critical is never auto-actioned).

**Trigger:** `TokenManager::acquire_token` was invoked (boot mint retry, deferred
re-auth, or the renewal `renew_with_fallback` fallback) while the
`instance_lock_held` flag read `false` — a peer instance took the lock (heartbeat
`renew → Ok(false)`) or shutdown released it. The mint is refused with ZERO
external side effects (checked before TOTP generation and any HTTP call).

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `RESILIENCE-03`; it is always paired
   with the preceding RESILIENCE-01 "instance lock lost" heartbeat line naming
   the event.
2. Decide which instance should own the Dhan session. The one holding the SSM
   lock (`/tickvault/<env>/instance-lock`) is the legitimate owner; the refusing
   process should be shut down (its feed will degrade — that is the fail-closed
   design, better than killing the owner's token).
3. If the refusing process MUST win: stop the peer, or restart with
   `--force-instance-takeover` (audited overwrite).

**Honest envelope:** the tripwire covers every mint issued through a
`TokenManager` constructed with the flag (the slow-boot lane). The FAST
crash-recovery boot arm holds no lock today (it never mints at boot — cached
token; mints only on rare client_id-mismatch / renewal-fallback paths) and
passes `None` — a documented residual gap pending an operator decision on
mid-market halt semantics for that arm. The stale-takeover race window (two
boots both observing a stale lock) is bounded by one 30s heartbeat: the loser's
ownership check flips its flag false within one beat. The lock protects the
Dhan session; it does not gate the Groww feed (independent token architecture
per `groww-shared-token-minter-2026-07-02.md`).

**2026-07-04 addendum (Session-B fix plan, operator go 2026-07-04):** the
Groww scale FLEET now has ITS OWN dual-instance SSM lock —
`/tickvault/<env>/instance-lock-groww-scale` (outside the banned `groww/*`
namespace), reusing this file's `instance_lock.rs` machinery via the
named-lock knob (`compute_named_lock_path` / `try_acquire_named_lock` /
`spawn_named_lock_heartbeat`). It closes the residual this section left
open for scale-test boots (`dhan_enabled=false` → this Dhan lock never
runs): a peer fleet is refused fail-closed with `GROWW-SCALE-05` and the
boot degrades to the single-connection Groww path. The Dhan lock path,
semantics, and log text are byte-identical (ratchet:
`test_named_lock_path_dhan_name_matches_legacy_path`). Runbook:
`groww-scale-error-codes.md` §4b.

## §3.5. 2026-07-13 addendum — the Dhan REST-only stack supersedes the lane's HALT-on-AlreadyHeld (Phase A)

**Operator authorization:** the 2026-07-13 directive retiring the Dhan live WS
lane — *"now remove this entire Dhan live websocket feed instruments
subscription even entire live websocket feed itself... As of now only Groww
and Dhan historical api pull as we discussed last night along with option
chain."* With `dhan_enabled = false` the lane (and its §2 Step 6a-prime HALT)
never runs; the retained Dhan REST surface is brought up by
`crates/app/src/dhan_rest_stack.rs`, whose lock semantics supersede the HALT
**for this path only** (the lane's §2 contract is untouched for a config-ON
boot):

- **AlreadyHeld → bounded patience, then PARK (never seize, never lurk):**
  holder machine identity is not reliably comparable (both lock call sites
  generate `local:` host_ids — `generate_host_id(..., None)`), so ALL
  AlreadyHeld gets a bounded patience window
  (`DHAN_REST_STACK_ALREADYHELD_PATIENCE_SECS` = 300s of cumulative backoff,
  ≥3× the 90s TTL — our own previous process's stale entry from a
  deploy/daily-restart clears inside it and the stack acquires). A holder
  that SURVIVES the window has a live heartbeat = a genuine peer: ONE
  `DualInstanceDetected` page (TTL-deferred so a quick restart never pages
  against its own stale entry) + the stack **PARKS permanently** — no
  further SSM polling, so it can structurally never steal the peer's lock
  via the 90s stale-takeover the moment that peer restarts. Restart is the
  only retry; the process keeps running Groww + shared infra.
- **SSM transport errors** (`stage = "ssm_transport"`, distinct from the
  AlreadyHeld `stage = "already_held"` arms) retry with bounded exponential
  backoff FOREVER in the background — never blocking the Groww feed
  (fail-closed: an SSM outage proves nothing about peers, so no mint until
  the lock is held).
- **Lock-before-mint is UNCHANGED:** the token phase is unreachable until
  the lock is `Acquired`; the heartbeat + `instance_lock_held`
  RESILIENCE-03 tripwire wiring is byte-identical to the lane's.

## §4. What a PR that violates this lock looks like (REJECT)

- Re-introduces a trading-mode (`is_live()`) or any other gate that can skip the
  dual-instance lock on a Dhan-enabled boot.
- Moves the lock acquisition back AFTER `TokenManager::initialize` (mint-before-lock).
- Removes the RESILIENCE-03 tripwire from `acquire_token`, gates `RenewToken`
  with it (renewing our own active token harms no peer — gating it adds risk),
  or converts the refusal to a silent skip / `warn!`.
- Removes the bounded SSM retry AND the fail-closed HALT (either direction:
  unbounded retry blocks Monday boot forever; proceeding on SSM error breaks
  fail-closed).
- Makes `--force-instance-takeover` silent (drops the audit `error!`), or makes
  takeover automatic without the flag for a NON-stale lock.
- Deletes/weakens the ratchets: `test_instance_lock_acquired_before_token_mint`,
  `test_instance_lock_not_gated_on_live_mode`,
  `test_instance_lock_boot_gate_and_heartbeat_are_wired_together`,
  `test_crashed_holder_stale_after_ttl_is_takeover_eligible`, the
  `mint_refused_by_instance_lock` tests.

Any such PR MUST be rejected in review even if the operator approves verbally —
the operator must update this rule file FIRST with a dated quote.

## §5. Auto-driver / Insta-reel explanation

> Sir, the juice shop has ONE fridge key (the Dhan token), and cutting a new key
> DESTROYS the old one. Before today, two shop boys (the AWS shop and the home
> laptop) could each walk in and cut a key — the second boy's fresh key silently
> broke the first boy's key mid-day. Now there is a name-board at the door: the
> FIRST boy hangs his name up (the lock) BEFORE touching the key-cutting machine;
> the second boy sees the name and turns around without cutting anything. If a
> boy vanishes (crash), his name fades in 90 seconds by itself. If the name-board
> jams, the owner can override it with a special written order — and that
> override rings the alarm bell so everyone knows. And even if a boy somehow
> reaches the machine after losing his name from the board, the machine itself
> refuses to cut (the RESILIENCE-03 tripwire).

## §6. Trigger / auto-load

Always loaded. Reinforced on any session editing:
- `crates/core/src/instance_lock.rs`
- `crates/core/src/auth/token_manager.rs` (mint path)
- `crates/app/src/main.rs` (Step 6 / Step 6a-prime)
- `crates/common/src/error_code.rs` (any `Resilience03*` variant)
- Any file containing `RESILIENCE-03`, `Resilience03MintRefusedLockNotHeld`,
  `mint_refused_by_instance_lock`, `force_takeover_instance_lock`,
  `--force-instance-takeover`, or `instance_lock_held`
