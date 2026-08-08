# Implementation Plan: Publish the minted Dhan access token to SSM (shared-token parity with Groww)

**Status:** APPROVED
**Date:** 2026-08-08
**Approved by:** Parthiban (operator) — 2026-08-08, verbatim: *"yes go ahead mtoherfucker okay? i jsut ened the fuckign workign solution"* + *"whichever is recommended go ahead dude okay?"*
**Authority:** `.claude/rules/project/groww-shared-token-minter-2026-07-02.md` §9 (this PR adds it)

---

## Design

### The problem, in one line

**Dhan permits exactly ONE active access token per account** (`docs/dhan-ref/02-authentication.md:216` — *"One active token at a time — generating new token invalidates the old one"*). tickvault mints a Dhan token via TOTP and **never publishes it**, so any second consumer of the same Dhan account must mint its own — which instantly invalidates tickvault's, whose watchdog then re-mints, invalidating the peer's. A flapping token war in which *last-to-mint wins*.

Groww already solved exactly this: **one minter, publishes to SSM, everyone else reads** (`groww-shared-token-minter-2026-07-02.md`). Dhan never got that treatment.

### Current state (Verified in source, 2026-08-08)

| Broker | tickvault mints? | Published to SSM? | tickvault reads? |
|---|---|---|---|
| Groww | ❌ no (mint deleted 2026-07-02) | ✅ by the bruteX Lambda | ✅ `secret_manager.rs:241 fetch_groww_access_token` |
| **Dhan** | ✅ TOTP → JWT (`token_manager.rs`, `totp_generator.rs`) | ❌ **NO — the gap** | n/a (it mints) |

The minted Dhan token lives ONLY in process memory (`arc-swap`) plus a local crash cache (`token_cache.rs` → `/tmp/tv-token-cache`, 0600, container-local, wiped on container restart). `grep put_parameter` across `crates/core/src/auth/` + `dhan_rest_stack.rs` returns **empty** — tickvault never writes it back.

### The fix

After every successful Dhan mint/renewal, **publish the token to SSM** as a `SecureString`, so any other consumer of the same Dhan account READS it instead of minting. Exactly one minter (tickvault) survives, so there is nothing left to invalidate.

**The parameter name is already reserved and unwired:** `deploy/aws/terraform/variables.tf:137` defines `dhan_access_token_ssm_param` defaulting to `/tickvault/prod/dhan/access-token`, with **zero consumers** anywhere in the repo.

### Placement — why a NEW module, not `secret_manager.rs`

`crates/common/tests/groww_no_mint_guard.rs:129` is a build-failing ratchet asserting `secret_manager.rs` contains **no** `put_parameter` — *"must remain read-only — no SSM put_parameter, ever."* That pin is correct and **stays**. The write therefore lands in a new sibling module `crates/core/src/auth/dhan_token_publisher.rs`, leaving the ratchet green and untouched.

### Fail-soft contract (non-negotiable)

The publish is **strictly best-effort and off the critical path**. tickvault already holds the token in memory; a publish failure must never degrade tickvault's own auth. Concretely: spawned (never awaited inside the mint), never panics, never retries in a tight loop, logs a coded `error!` + increments a counter on failure, and **never logs the token value**.

### Hook points

Both post-mint sites in `token_manager.rs`, immediately after the existing cache save:
- `:846` (initial `generateAccessToken` path)
- `:929` (renewal path)

---

## Plan Items

- [x] Add `DHAN_ACCESS_TOKEN_SECRET` constant
  - Files: `crates/common/src/constants.rs`
  - Tests: `test_dhan_access_token_secret_constant`

- [x] New fail-soft publisher module (SSM `PutParameter`, `SecureString`, `Overwrite=true`)
  - Files: `crates/core/src/auth/dhan_token_publisher.rs`, `crates/core/src/auth/mod.rs`
  - Tests: `test_publish_never_panics_on_client_error`, `test_publish_request_shape_is_securestring_overwrite`, `test_token_value_never_appears_in_logs`

- [x] Wire both post-mint sites (spawned, never awaited)
  - Files: `crates/core/src/auth/token_manager.rs`
  - Tests: `test_both_mint_sites_publish`

- [x] Ratchet: publish stays out of `secret_manager.rs`; both sites wired; fail-soft preserved
  - Files: `crates/core/tests/dhan_token_publish_guard.rs`
  - Tests: `secret_manager_stays_read_only`, `both_mint_sites_publish`, `publish_is_spawned_not_awaited`, `publisher_never_logs_token_value`

- [x] Terraform: **NO GRANT CHANGE NEEDED — corrected after verification.** The instance role ALREADY holds `ssm:PutParameter` on `arn:...:parameter/tickvault/${var.environment}/*` (`main.tf`, added for the dual-instance lock), which covers `/tickvault/prod/dhan/access-token`. No KMS grant either: the param is a SecureString under the DEFAULT `aws/ssm` key (the existing `kms:Decrypt` statement is Groww-specific — that param uses the customer-managed `alias/tickvault-groww` CMK). Only a comment was added recording that PutParameter is now load-bearing for TWO callers.
  - Files: `deploy/aws/terraform/main.tf` (comment only)
  - Tests: n/a — no policy change to pin

- [x] Rule-file §9 — the Dhan shared-token contract + honest envelope
  - Files: `.claude/rules/project/groww-shared-token-minter-2026-07-02.md`
  - Tests: n/a (docs; the §9 phrases are pinned by the guard above)

---

## Edge Cases

| # | Case | Handling |
|---|---|---|
| 1 | SSM `PutParameter` fails (AccessDenied / throttle / network) | Coded `error!` + counter; tickvault keeps its in-memory token; **no retry storm**, next mint republishes |
| 2 | KMS key unavailable | Same as #1 — fail-soft |
| 3 | Two tickvault instances (dual-instance lock) both publish | Idempotent `Overwrite=true`; the dual-instance SSM lock already prevents concurrent minting |
| 4 | Token published then immediately renewed | Last write wins; readers re-read on auth failure (the Groww consumer pattern) |
| 5 | A reader holds a stale token across a renewal | Reader's responsibility — re-read on 401, exactly as tickvault does for Groww (`_is_auth_error` → drop → re-read) |
| 6 | Publish latency delays the mint | Impossible — spawned, never awaited |
| 7 | Token value leaks to logs | `Secret<String>` + no value in any log field; ratcheted |
| 8 | Param does not exist yet on first publish | `PutParameter` with `Overwrite=true` creates it |
| 9 | Sandbox/dev environment | Path is environment-scoped via `build_ssm_path` — dev writes `/tickvault/dev/...` |

---

## Failure Modes

| Mode | Blast radius | Detection | Mitigation |
|---|---|---|---|
| Publish silently never runs | Peer keeps minting → token war persists | counter stays at 0 | ratchet pins both call sites |
| Publish writes the WRONG value | Peer authenticates as nothing → its calls 401 | peer-side auth errors | value comes straight from the same `TokenState` the cache uses |
| IAM grant too broad | Least-privilege regression | terraform review | N/A — no new grant; the existing `/tickvault/<env>/*` PutParameter (dual-instance lock) already covers it, and a comment now records the second caller |
| Publish blocks the mint | Auth boot stalls → prod data pulls stop | boot deadline (Step 6, 60s) | spawned, never awaited; ratcheted |
| Token value in CloudWatch logs | **Credential disclosure** | secret-scan + ratchet | `Secret<String>`, no value logged, ratcheted |

---

## Test Plan

- Unit: publisher request shape (`SecureString`, `Overwrite=true`, correct path), fail-soft on error, no token in logs.
- Integration/source-scan ratchet (`dhan_token_publish_guard.rs`): both mint sites wired; `secret_manager.rs` still `put_parameter`-free; publish spawned not awaited.
- Existing suites must stay green — notably `groww_no_mint_guard.rs` (all 6, incl. the `:129` read-only pin) and the auth suite.
- Scope: `crates/common` changes → escalate to `cargo test --workspace` per `testing-scope.md`.
- **Live verification is NOT claimed from CI** — the real proof is a prod boot writing the param, checked with `aws ssm get-parameter`. Recorded as a post-merge operator step, not asserted here.

---

## Rollback

Single revert restores today's behaviour exactly — tickvault keeps minting and simply stops publishing; **nothing else consumes the param**, so no reader breaks that wasn't already broken. The IAM grant can be revoked independently (the publish then fails soft per Edge Case #1, which is the pre-change state). No schema, no data migration, no config flag flip required. The publish is additive: **removing it cannot break tickvault's own auth**, because tickvault never reads the param it writes.

---

## Observability

| Layer | Artefact |
|---|---|
| Counter | `tv_dhan_token_publish_total{outcome="ok"\|"error"}` |
| Log (success) | `debug!` — param path only, **never the value** |
| Log (failure) | `error!` with `code = ErrorCode::…code_str()` + the AWS error class |
| Audit | n/a — no new event table (the token lifecycle is already audited) |
| Alert | none added — a publish failure is peer-visible and fail-soft; deliberately NOT a new Dhan Telegram page (the 4-item family is locked by `dhan-rest-only-noise-lock-2026-07-14.md` §2, which forbids adding one without a dated quote) |

---

## Z+ 15-row / 7-row guarantee matrices

Carried by reference from `.claude/rules/project/per-wave-guarantee-matrix.md`. Item-specific notes:

- **Code coverage** — every new `pub fn` has a test + a call site.
- **Performance** — cold path only (mint happens ~once/day + renewals). Zero hot-path involvement; no DHAT needed.
- **Security / hardening** — this is the item's core risk. `Secret<String>`, never logged, `SecureString` at rest, IAM scoped to one param ARN. security-reviewer agent pass required.
- **Uniqueness + dedup** — n/a (single param, not a data table).
- **Zero data loss / WS / QuestDB / O(1)** — untouched; no tick path, no WS, no DB.
- **Resilience** — the publish cannot degrade tickvault's own auth (fail-soft, spawned, ratcheted).

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the publish is **fail-soft by construction** (spawned, never awaited, never panics — pinned by `dhan_token_publish_guard.rs`), the token value never reaches a log (pinned), and `secret_manager.rs` stays read-only (pinned by the pre-existing `groww_no_mint_guard.rs:129`). **NOT claimed:** that the param is readable by any specific peer — that is an IAM grant on the *reader* side, outside this repo; nor that a peer will stop minting — that is the peer's change. This PR removes the *cause* (an unshared token); it cannot force a peer to consume it. Live SSM write behaviour is UNVERIFIED-IN-CI (no AWS creds in CI) and is confirmed by an operator `aws ssm get-parameter` after the first prod boot.
