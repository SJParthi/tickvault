# Groww Shared Token-Minter — Consumer-Only Lock (Operator 2026-07-02)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §I > `groww-second-feed-scope-2026-06-19.md` > this file > `no-rest-except-live-feed-2026-06-27.md` (Groww-auth row SUPERSEDED by this file) > defaults.
> **Scope:** PERMANENT. Every environment, every PR, every future Claude/Cowork session.
> **Operator-locked:** 2026-07-02 (verbatim directive below).
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The verbatim operator directive (preserve exactly, do not paraphrase)

> "GROWW TOKEN ARCHITECTURE SYNC — TickVault must adopt the SHARED AWS token system without breaking it. … ONE MINTER ONLY: an AWS Lambda `groww-token-minter` is the SOLE component that ever mints a Groww access token. It runs daily via EventBridge cron(35 0 * * ? *) (~06:05 IST, right after Groww's 06:00 IST daily token reset), reads the api-key + totp-secret FRESH from SSM on every invocation, mints via TOTP, and writes the token to SSM. … CONSUMERS ARE READ-ONLY: BruteX and TickVault each have a dedicated IAM reader role (groww-token-minter-reader-tickvault for you) scoped to ssm:GetParameter on the access-token parameter ONLY + kms:Decrypt. … NEVER mint a Groww access token from TickVault code, in any environment, ever. … NEVER write to any /tickvault/prod/groww/* parameter. … NEVER deploy a second minter/Lambda/terraform copy. … NEVER cache the token past an auth failure. On any 401/auth-expired from Groww: re-read /tickvault/prod/groww/access-token from SSM and retry with the fresh value, bounded retry with backoff (~60s interval, up to ~10 min) … If still stale after the bound, alert — do not mint. … on reconnect/re-subscribe, re-read the token from SSM and rebuild GrowwAPI + GrowwFeed. Never pass credentials to the feed — it only takes the token. No local .env/keychain copies of Groww credentials in TickVault production paths. The token may be held in memory only."

---

## §1. The architecture (authoritative, bruteX-owned)

| Component | Owner | Role |
|---|---|---|
| Lambda `groww-token-minter` (ap-south-1, acct 208384284948, `deploy/aws_token_minter/` in bruteX) | bruteX repo (terraform, S3 remote state) | THE SOLE MINTER — daily ~06:05 IST, reads credentials fresh, TOTP-mints, writes the token |
| `/tickvault/prod/groww/api-key` + `/totp-secret` (SecureString, KMS `alias/tickvault-groww`) | Lambda-only read; operator writes on rotation | Credentials — TickVault can NEITHER read NOR write them (IAM) |
| `/tickvault/prod/groww/access-token` | Written by the Lambda; read by consumers | The shared daily token |
| IAM role `groww-token-minter-reader-tickvault` | consumed by TickVault | `ssm:GetParameter` on the access-token param ONLY + `kms:Decrypt` |

Credential rotation is invisible to TickVault: the operator pastes new credentials into the two SSM params once; the next Lambda mint picks them up; consumers notice nothing.

## §2. TickVault's implementation (this lock's landing PR)

| Surface | Behaviour |
|---|---|
| `crates/core/src/auth/secret_manager.rs::fetch_groww_access_token` | READ-ONLY GetParameter(decrypt) of the access-token param via the default AWS credential chain (the reader role on the box); no writes exist in the module (ratcheted) |
| `crates/core/src/feed/groww/auth.rs` | Mint DELETED. `run_groww_auth_smoke_check` = SSM read + pure `validate_groww_token_shape`; empty/placeholder → feed page shows "minter has not populated the token" *(2026-07-15: file DELETED with the Groww live feed — row retained as historical audit; the machinery rows for `secret_manager.rs` stand — the REST legs remain the read-only consumers)* |
| `crates/app/src/groww_sidecar_supervisor.rs` | NO credential fetch, NO secret in the child env — injects only `GROWW_SSM_TOKEN_PARAM` (a path string). Requirements-fingerprint marker re-provisions the venv when deps change *(2026-07-15: file DELETED with the Groww live feed — row retained as historical audit; the machinery rows for `secret_manager.rs` stand — the REST legs remain the read-only consumers)* |
| `scripts/groww-sidecar/groww_sidecar.py` | Reads the token via boto3 GetParameter(WithDecryption) each auth cycle (env `GROWW_ACCESS_TOKEN` override for local dev — token only, never credentials). Cached across pure feed-side reconnects; dropped on ANY auth-class failure (`_is_auth_error`: 401/403/auth-shaped — how the 06:00 reset surfaces on connect/subscribe) so the NEXT cycle re-reads SSM and rebuilds `GrowwAPI(access_token)` + `GrowwFeed(groww)`. Auth-stale retries floored at `AUTH_RETRY_FLOOR_SECS = 60`; after `TOKEN_STALE_ALERT_SECS = 600` of continuous auth failure, ONE edge-triggered `GROWW LIVE FEED REJECTED: access token stale …` line (supervisor routes → feed-health Down + Telegram) and retries continue at the ceiling — NEVER a mint *(2026-07-15: file DELETED with the Groww live feed — row retained as historical audit; the machinery rows for `secret_manager.rs` stand — the REST legs remain the read-only consumers)* |
| `scripts/aws-seed-ssm-parameters.sh` | Seeds NOTHING under `groww/*` (the Lambda owns every groww param) |
| `scripts/groww-sidecar/requirements.txt` | `boto3` pinned; the TOTP dependency removed with the mint path *(2026-07-15: file DELETED with the sidecar — historical)* |

## §3. What a PR that violates this lock looks like (REJECT)

- Re-introduces any call to Groww's token-mint REST endpoint, any Groww TOTP computation, or the deleted `obtain_groww_access_token`/`fetch_groww_credentials`/`GrowwCredentials` surface.
- Reads `groww/api-key` or `groww/totp-secret` from anywhere in TickVault.
- Writes (put_param / put-parameter) ANY `/tickvault/*/groww/*` parameter from this repo.
- Adds a second minter Lambda / terraform copy / EventBridge cron for Groww tokens.
- Caches the token past an auth failure (removes the `_is_auth_error` token-drop) or removes the 60s auth pacing / 10-min stale alert.
- Passes credentials (not the token) to `GrowwAPI`/`GrowwFeed`.
- Adds a local `.env`/keychain copy of Groww credentials to a production path.

Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this rule file FIRST with a dated quote.

## §4. Mechanical ratchets (build-failing)

| Ratchet | Pins |
|---|---|
| `crates/common/tests/groww_no_mint_guard.rs` (6 tests) | No mint endpoint / mint fn / credential fetch / credential type in any `crates/*/src`; no credential param paths; sidecar reads `GROWW_SSM_TOKEN_PARAM` + `WithDecryption=True`, never mints, carries the 60s floor + 600s alert marker; smoke script mint-free; requirements pins boto3 not the TOTP lib; `fetch_groww_access_token` exists and its module has no `put_parameter` |
| `crates/common/tests/groww_ssm_seed_script_wiring.rs` (5 tests) | Seed script writes NO groww param, documents the minter ownership, carries no Groww credential env vars; the Rust reader targets the access-token param only |
| `crates/app/src/groww_sidecar_supervisor.rs` unit tests | Supervisor injects `GROWW_SSM_TOKEN_PARAM`, never `GROWW_API_KEY`/`GROWW_TOTP_SECRET`; requirements-fingerprint re-provisioning gate |
| `crates/core/src/feed/groww/auth.rs` unit tests | `validate_groww_token_shape` (empty/placeholder/short/JWT + no-value-echo) |

## §5. Honest envelope (mandatory per operator-charter §F)

> "100% inside the tested envelope: TickVault holds the Groww token in memory only, read read-only from the minter's SSM parameter; on auth failure it re-reads (never mints) at ≥60s pacing and alerts once after ~10 min while continuing to retry. TickVault does NOT control the minter — if the bruteX Lambda stops minting, Groww stays down until the Lambda runs (the alert names it); that is the designed division of ownership, not a TickVault defect. Whether the box's instance profile IS the reader role (vs an explicit sts:AssumeRole hop) is AWS-side wiring verified on the live box, not provable from this repo."

## §6. Auto-driver / Insta-reel explanation

> Sir, the juice shop's fridge has ONE key, and only the head office (bruteX) cuts new keys — every morning at 6:05 they put the fresh key in the shared locker. Our shop boy just TAKES the key from the locker; he is physically unable to cut keys and unable to open the credentials drawer. If the key doesn't work (6:00 changeover), he calmly checks the locker again every minute; after ten minutes he phones you ONCE ("head office hasn't cut today's key") and keeps checking — he never tries to cut his own key, because two shops cutting keys at once breaks BOTH shops' locks.

## §7. Trigger / auto-load

Always loaded. Reinforced on any session editing `crates/core/src/feed/groww/auth.rs`, `crates/core/src/auth/secret_manager.rs`, `crates/app/src/groww_sidecar_supervisor.rs`, `scripts/groww-sidecar/*`, `scripts/aws-seed-ssm-parameters.sh`, or any file containing `groww-token-minter`, `GROWW_SSM_TOKEN_PARAM`, `GROWW_ACCESS_TOKEN_SECRET`, or `fetch_groww_access_token`.

---

## §8. 2026-08-08 — OPERATOR SCOPE CHANGE: tickvault to mint Groww via TOTP (native, both brokers self-sufficient)

> **This section SUPERSEDES the §1/§2/§3 consumer-only contract** for the mint
> capability, per the dated operator directive below. Sections §1-§7 are retained
> as the 2026-07-02 historical record (house convention: annotate, never rewrite).
> **Execution is GATED on the §8.4 probe** — the authorization is recorded here
> FIRST (rule-file-first law), the code lands only after §8.4 returns a verdict
> and the companion plan is operator-APPROVED (design-first wall).

### §8.0 The verbatim operator demands (preserve EXACTLY, typos + expletives included)

**Quote 1 (2026-08-08):**
> "reoslve the dhan issue bro so both groww and dhan totp shdou lw ork rihg t bro am i irght dude?"

**Quote 2 (2026-08-08, same session — the scope correction):**
> "forget abotu brutex mtoherfucekr i asked you to make it dhan and gorww broekr totp bro okay?"

**Quote 3 (2026-08-08, same session):**
> "what si the fuckign issue ddue how shdou lw e reoslve both vendors totp issue bro okay?"

Quote 2 explicitly withdraws the brutex-sharing framing an earlier turn of that
session had (wrongly) assumed. The operator's scope is: **tickvault performs TOTP
for BOTH brokers itself** — Dhan (already does) and Groww (does not).

### §8.1 The measured starting state (Verified 2026-08-08, in source)

| Broker | TOTP in tickvault | Evidence |
|---|---|---|
| **Dhan** | ✅ **PRESENT AND WORKING** | `crates/core/src/auth/totp_generator.rs`; `token_manager.rs:34` `use super::totp_generator::generate_totp_code`, with a TOTP retry ladder (`:323` `totp_retries`, `:362` `is_totp_error`, `TOTP_MAX_RETRIES`). Mint = `TokenManager::initialize` (SSM creds -> TOTP -> JWT), spawned UNCONDITIONALLY by `dhan_rest_stack` and consumed at fire time by `DhanCadenceExecutor`. |
| **Groww** | ❌ **ABSENT** | Zero TOTP tokens anywhere under `crates/core/src/feed/groww/` or `crates/trading/src/oms/groww/`. tickvault only READS a pre-minted token: `secret_manager.rs:241 fetch_groww_access_token`. The mint path was DELETED 2026-07-02 by §2 of this file. |

**So "the Dhan issue" is not a Dhan defect — Dhan is the half that already works.
The gap is Groww.** (An earlier turn of the 2026-08-08 session mis-scoped this as a
brutex token-sharing problem; that reading is withdrawn by Quote 2 and recorded here
only so the correction is auditable.)

### §8.2 Groww HAS a TOTP mint flow (Verified — this is achievable)

`docs/groww-ref/17-token-lifecycle.md` §1 documents **three** Groww auth approaches;
the 3rd is the **TOTP Flow**: `POST /v1/token/api/access` with `key_type: "totp"`,
using API Key + TOTP code. This is exactly what the bruteX Lambda already does
("mints via TOTP", §1 of this file), so the flow is proven in production — just not
from tickvault.

Note the documented contradiction (`17-token-lifecycle.md` §5): the REST page marks
the TOTP flow *"Requires daily approval on Groww Cloud Api keys page"* while the
python-sdk page marks it *"(Uses TOTP token and TOTP QR code — **No Expiry**)"*.
Unresolved; §8.4 probes it.

### §8.3 The four real blockers (one is an IAM hard-stop, not a code gap)

| # | Blocker | Nature | Resolution |
|---|---|---|---|
| 1 | **IAM hard-stop** — tickvault's reader role (`groww-token-minter-reader-tickvault`) is scoped to `ssm:GetParameter` on the **access-token param ONLY** + `kms:Decrypt`. The `groww/api-key` + `groww/totp-secret` params are **Lambda-only read** (§1). | **Runtime AccessDenied**, not a code gap — perfect TOTP code would still fail. | terraform/IAM grant: add the 2 credential params to tickvault's reader policy. |
| 2 | **This file's own §1/§3** — verbatim: *"NEVER mint a Groww access token from TickVault code, in any environment, ever."* | Governance | THIS §8 is that override, with the §8.0 dated quotes. |
| 3 | **6 build-failing tests** — `crates/common/tests/groww_no_mint_guard.rs` bans *"any Groww TOTP computation"*, the `obtain_groww_access_token` / `fetch_groww_credentials` / `GrowwCredentials` surface, credential param paths, and `put_parameter` in `secret_manager.rs` (`:129`). | Ratchet | Re-bless in the SAME PR as the code, with this §8 cited as authority. The `secret_manager.rs` read-only pin should be KEPT (put the mint in its own module). |
| 4 | **Deleted code** — the Groww mint path existed pre-2026-07-02. | Recovery | Restore from git history (pre-`dd7eaa5e^` lineage), then re-harden. |

### §8.4 THE GATING PROBE — Groww one-active-token semantics (UNKNOWN, must run FIRST)

**Why this gates everything:** if Groww invalidates the previous token on each mint
(as **Dhan explicitly does** — `docs/dhan-ref/02-authentication.md:216`), then tickvault
minting its own Groww token would kill the bruteX Lambda's token and vice-versa — a
flapping token war that breaks the Groww REST legs.

**The evidence LEANS SAFE but is explicitly UNVERIFIED.**
`docs/groww-ref/17-token-lifecycle.md:122` (Unknown #2) records verbatim:

> "**One-active-token semantics** | Nothing in the captured docs or SDK states whether
> minting a new access token invalidates the previous one (contrast: Dhan documents
> one-active-token explicitly). The web UI supports multiple named tokens ("create,
> revoke and manage all your tokens"); `sessionName`/`isActive` in §4 hint at
> multi-session support. Empirically BruteX + TickVault concurrently USE one shared
> minted token without conflict. | **Unknown** — probe: mint twice in succession;
> test whether token #1 still authenticates."

**The probe (exactly as the doc names it), OFF-HOURS, operator-triggered:**

1. Mint Groww token **A** via the TOTP flow. Record it.
2. Mint Groww token **B** via the same flow. Record it.
3. Authenticate a cheap read with **A**.
4. **A still works => Groww is multi-token => tickvault may mint safely alongside the
   Lambda.** **A is dead => one-active-token => tickvault must NOT mint concurrently**;
   fall back to mint-only-when-SSM-token-is-stale/absent, or retire the Lambda.

Run OUTSIDE market hours, never against the in-session token. Until the verdict is
recorded HERE, **no Groww mint code may ship** (the §8.5 REJECT rows bind).

### §8.5 What a PR that violates THIS §8 looks like (REJECT)

- Ships ANY Groww mint code before the §8.4 probe verdict is recorded in this file.
- Grants tickvault read on the Groww credential params without the §8.4 verdict
  (the IAM grant is the point of no return — it makes an accidental mint possible).
- Puts the mint's SSM write (if any) into `secret_manager.rs` — that module's
  read-only pin (`groww_no_mint_guard.rs:129`) STAYS; use a separate module.
- Weakens the Groww mint's TOTP/credential hygiene below the Dhan bar: credentials
  read FRESH from SSM per mint, held only as `Secret<String>`, NEVER logged, never in
  a URL, never written to a local `.env`/keychain.
- Removes the SSM-read fallback path (`fetch_groww_access_token`) — after §8 tickvault
  should PREFER the shared minted token and mint only as the documented fallback,
  so a healthy Lambda day costs zero extra mints.
- Leaves the bruteX Lambda and a tickvault mint racing without the §8.4 verdict
  proving that is safe.

Any such PR MUST be rejected in review even if the operator approves verbally — the
§8.4 verdict must be recorded HERE first.

### §8.6 Honest envelope (mandatory per operator-charter §F)

> "Recorded, not claimed: Dhan TOTP is Verified working in tickvault today (source
> evidence, §8.1). Groww TOTP is Verified ABSENT and Verified ACHIEVABLE (the flow
> exists and the Lambda proves it in prod, §8.2). NOT claimed: that tickvault can mint
> Groww today — it CANNOT, by IAM (§8.3 #1), and that is a runtime denial no code fixes.
> NOT claimed: that two concurrent Groww minters are safe — that is **Unknown**, leaning
> safe on multi-named-token evidence, and §8.4 is the probe that settles it. NOT claimed:
> the TOTP flow's true expiry — the REST page ('daily approval') and the python-sdk page
> ('No Expiry') contradict each other and §8.4 probes it. No code ships on an Unknown."

### §8.7 Auto-driver / Insta-reel explanation

> Sir, our shop cuts its OWN key for fridge Dhan every morning — that half already
> works perfectly. For fridge Groww we never learned to cut a key; we just borrow head
> office's. You want our boy able to cut BOTH keys himself. Two problems: (1) the Groww
> key-cutting machine is LOCKED to him — he isn't allowed to open the drawer with the
> Groww key-blank, so teaching him is useless until you unlock that drawer; and (2) for
> the Dhan fridge we KNOW a second key kills the first, but for Groww nobody has ever
> checked — and the signs look good (that fridge seems to accept several named keys at
> once). So: cut two Groww keys back to back, then test the FIRST one. Still opens? Our
> boy can cut his own key safely. Dead? Then only one of us cuts Groww keys, ever.

### §8.8 Trigger / auto-load

The §7 trigger list covers this section. Additionally reinforced on any session
editing `crates/common/tests/groww_no_mint_guard.rs`, `crates/core/src/auth/totp_generator.rs`,
`deploy/aws/terraform/*groww*`, or any file containing `key_type`, `token/api/access`,
`obtain_groww_access_token`, or `GrowwCredentials`.

---

## §9. 2026-08-08 — THE DHAN SHARED-TOKEN CONTRACT (tickvault mints AND publishes)

> **This is the SHIPPED resolution of the §8.0 operator directive.** §8 recorded
> the goal ("both brokers' TOTP working"); §9 records what actually closes it.
> **The §8.2 path — teaching tickvault to mint Groww natively — is NOT needed and
> is DEFERRED**: tickvault's Groww token already works (it reads the Lambda's), and
> its Dhan token already works (it mints its own). The single real breakage was that
> tickvault's Dhan token was never SHARED. §8 stays as the recorded authorization
> and the §8.4 probe remains the gate on any future native Groww mint.

### §9.0 The diagnosis (Verified 2026-08-08)

**"One active token at a time — generating new token invalidates the old one"**
(`docs/dhan-ref/02-authentication.md:216`). Dhan is single-token PER ACCOUNT.

tickvault mints a Dhan token via TOTP and kept it entirely private — process
memory (`arc-swap`) plus `/tmp/tv-token-cache` (0600, container-local, wiped on
container restart). `grep put_parameter` across `crates/core/src/auth/` +
`dhan_rest_stack.rs` returned **empty**: it never wrote the token back.

So any peer consumer of the same Dhan account had to mint its own — which
invalidated tickvault's token; tickvault's mid-session watchdog then re-minted,
invalidating the peer's; repeat. **A flapping re-mint war where last-to-mint wins.**
Neither side's TOTP was faulty — the ACCOUNT is single-token and had two minters.

**Therefore rotating the TOTP secret fixes nothing** (same account, same limit) and
would break both sides equally. Recorded because it is the intuitive wrong fix.

### §9.1 The contract (LOCKED) — one minter per broker, the other side reads

| Broker | Minter (exactly one) | Publishes to SSM | tickvault's role |
|---|---|---|---|
| Groww | bruteX Lambda (~06:05 IST) | `/tickvault/<env>/groww/access-token` | **reader** (`secret_manager::fetch_groww_access_token`) — never mints (§1–§3 unchanged) |
| **Dhan** | **tickvault** (TOTP → JWT) | **`/tickvault/prod/dhan/access-token`** | **minter + publisher** (`crates/core/src/auth/dhan_token_publisher.rs`) |

Perfectly symmetric with the roles reversed. Each broker has exactly ONE minter, so
there is nothing left to invalidate.

**The parameter name was already reserved and unwired** — `deploy/aws/terraform/variables.tf`
`dhan_access_token_ssm_param` defaulted to `/tickvault/prod/dhan/access-token` with
ZERO consumers repo-wide. This section wires the intent that was already recorded.

### §9.2 Mechanical contract

| Aspect | Locked value |
|---|---|
| Write site | `crates/core/src/auth/dhan_token_publisher.rs` — the ONLY SSM write in the auth tree |
| Read-only pin PRESERVED | `secret_manager.rs` stays `put_parameter`-free (`groww_no_mint_guard.rs:129` + the new `dhan_token_publish_guard.rs::secret_manager_stays_read_only`) |
| Trigger | after EVERY successful mint AND renewal, paired 1:1 with the existing crash-cache save |
| Execution | **spawned, never awaited** — a slow/hanging SSM call can never delay a mint or stall the Step-6 boot deadline |
| Failure policy | **fail-soft**: no panic, no retry loop, coded `error!` + `tv_dhan_token_publish_total{outcome}`; tickvault's own auth is unaffected (token is in memory) |
| Secrecy | `Secret<String>`; written as `SecureString`; the VALUE never reaches a log line (ratcheted) |
| IAM | **NO grant change needed (Verified 2026-08-08).** The instance role ALREADY holds `ssm:PutParameter` on `arn:aws:ssm:<region>:*:parameter/tickvault/<env>/*` (`main.tf`, granted for the dual-instance lock), which covers this parameter. No KMS grant either — it is a SecureString under the DEFAULT `aws/ssm` key; the existing `kms:Decrypt` statement is Groww-specific (that param uses the customer-managed `alias/tickvault-groww` CMK owned by bruteX). Only a comment was added recording that `PutParameter` is now load-bearing for TWO callers, so a future least-privilege narrowing checks both |
| Ratchet | `crates/core/tests/dhan_token_publish_guard.rs` (8 build-failing tests) |

### §9.3 What a PR that violates §9 looks like (REJECT)

- Removes the publish from either mint site, or lets the publish/cache-save pair drift.
- Awaits the publish on the mint path (re-introduces a boot-stall surface).
- Moves the SSM write into `secret_manager.rs` (breaks the read-only pin).
- Logs the token value, or adds a second `expose_secret()` in the publisher.
- Points the publish at the Groww service segment (the secret NAME is identical —
  `access-token` — so only the service segment prevents overwriting the Groww token
  and 401-ing the Groww REST legs).
- Broadens the IAM grant beyond that single parameter ARN.
- Adds a SECOND Dhan minter anywhere (that is the exact defect this section closes).

### §9.4 Honest envelope (mandatory per operator-charter §F)

> "100% inside the tested envelope, with ratcheted regression coverage: the publish is
> fail-soft BY CONSTRUCTION (spawned, never awaited, no panic path — all pinned), the
> token value never reaches a log (pinned), `secret_manager.rs` stays read-only (pinned
> twice), and both mint sites publish (pinned). **NOT claimed:** that any peer actually
> reads it — that is an IAM grant plus a code change on the PEER side, outside this
> repo; this removes the peer's REASON to mint, it cannot force the peer to stop. **NOT
> claimed:** live SSM write behaviour — CI has no AWS credentials, so the first prod
> boot plus an operator `aws ssm get-parameter --name /tickvault/prod/dhan/access-token`
> is the real proof. **NOT claimed:** that Groww native TOTP now exists in tickvault —
> it does not (§8 defers it; tickvault reads the Lambda's Groww token as before)."

### §9.5 Auto-driver / Insta-reel explanation

> Sir, the Dhan fridge accepts only ONE key at a time — cut a second key and the first
> one stops working. Our shop cuts a Dhan key every morning and kept it in its own
> pocket. So when the neighbour cut his own Dhan key, ours died; our boy noticed and
> re-cut, which killed the neighbour's; round and round all day. Nobody's key-cutting
> was faulty — the fridge simply allows one key. Fix: our shop keeps cutting the Dhan
> key (it is good at it) and now **drops a copy in the shared locker** — exactly what
> head office already does for the Groww fridge. The neighbour takes the copy instead
> of cutting, so nothing ever kills anything again. And note: getting a NEW key blank
> would not have helped — same fridge, same one-key rule.

### §9.6 Trigger / auto-load

The §7 trigger list applies. Additionally reinforced on any session editing
`crates/core/src/auth/dhan_token_publisher.rs`, `crates/core/src/auth/token_manager.rs`,
`crates/core/tests/dhan_token_publish_guard.rs`, or any file containing
`DHAN_ACCESS_TOKEN_SECRET`, `tv_dhan_token_publish_total`, or
`dhan_access_token_ssm_param`.

---

## §10. 2026-08-08 — THE DHAN TOKEN MINTER LAMBDA (box-independent; supersedes §9's minter placement)

> **This section SUPERSEDES §9's "tickvault is the Dhan minter" placement.**
> §9 correctly identified the defect (the Dhan token was never published) and
> shipped the publish. §10 fixes a SECOND defect §9 did not address: §9 left
> the MINTER on the prod EC2 box, so token freshness stayed coupled to the box
> being up. §9's publisher code is RETAINED and is not deleted — see
> "Coexistence" below. Sections §1–§9 remain the historical record.

### §10.0 The verbatim operator authorization (2026-08-08)

> "build the lambda"

Given in direct response to a message proposing exactly this: a Dhan token
minter Lambda mirroring the Groww one, so token freshness stops depending on
the EC2 box. Reconfirmed the same session: "yes yes yes".

### §10.1 The incident this closes (Verified, live evidence)

| Fact | Evidence |
|---|---|
| Dhan permits ONE active token per account | `docs/dhan-ref/02-authentication.md:216` |
| The Dhan token parameter had not been written since **25 July** | brutex-side `describe`/console read, 2026-08-08 |
| The Groww parameter refreshed normally the same morning (00:35 GMT) | same read — the asymmetry IS the finding |
| Prod box unstartable since ~2026-08-06 (`InsufficientInstanceCapacity`, ap-south-1a) | Aug 5 = 0h, Aug 7 = 0h CPU; `daily-universe-scope-expansion-2026-05-27.md` §0 Quote 12 |
| Consumers reading the stale parameter got `HTTP 401 / DH-901` on **785 of 785** attempts | brutex session, 2026-08-08 |

**Root cause:** the only Dhan minter ran on the box. Box down ⇒ no mint ⇒ the
parameter froze ⇒ every consumer served a dead token. Groww was immune because
its minter is a Lambda. **Rotating the TOTP secret would have fixed nothing**
(same account, same one-token limit) and would have broken both brokers —
recorded because it is the intuitive wrong fix.

### §10.2 The contract (LOCKED)

| Aspect | Locked value |
|---|---|
| Minter | `tv-<env>-dhan-token-minter` Lambda (`crates/aws-lambdas/src/dhan_token_minter.rs`) — **the sole Dhan minter** |
| Schedule | `cron(35 0 * * ? *)` = **06:05 IST daily, EVERY day** (not MON-FRI — consumers read this parameter on weekends too) |
| Reads | `/tickvault/<env>/dhan/{client-id,client-secret,totp-secret}` — enumerated ARNs, never a `/dhan/*` wildcard |
| Writes | `/tickvault/<env>/dhan/access-token` ONLY, `SecureString`, `Overwrite=true` — scoped to that single ARN so it can never clobber the Groww token (same secret name, different service segment) |
| Language | native Rust (`rust-only-forever-lock-2026-07-19.md`); TOTP via the EXISTING `totp-rs` workspace pin, same SHA-1/6-digit/30s parameters as `crates/core/src/auth/totp_generator.rs` — no new dependency root |
| EC2 permissions | **NONE.** It mints and publishes; it never starts, stops or describes the box |
| Secret hygiene | token/PIN/TOTP never logged (length + shape verdict only); the mint URL is logged without its query string, which carries the PIN; `reqwest` transport errors are reported by KIND because their `Display` embeds the full URL |
| Fail-loud | every failure path returns `Err` ⇒ Lambda invocation error ⇒ `tv-<env>-dhan-token-minter-errors` pages. A malformed/error-body token is REFUSED **before** the SSM write, so a bad mint can never overwrite a good token |
| Schedule-drop detection | `tv-<env>-dhan-token-minter-not-invoked` (`Invocations < 1` per 24h, `treat_missing_data=breaching`) — the Errors alarm is blind to a dropped schedule (no invocation = no error), the 2026-07-02 repo-wide scheduler-drop class |

### §10.3 Coexistence with §9 (IMPORTANT — read before changing either)

§9's publisher (`crates/core/src/auth/dhan_token_publisher.rs`) is **retained
and still wired**. Today that is safe because the box is down, so only one
minter can actually run. **It is NOT safe once the box starts:** two minters
against a one-active-token account is precisely the re-mint war §9 set out to
end, re-created from the other direction.

**REQUIRED before the box next runs** (flagged, NOT done in this PR): tickvault
must READ this parameter instead of minting — the same posture it already has
for Groww (`fetch_groww_access_token`). Until that lands, a running box plus
this Lambda will fight. Stated plainly rather than left implicit; no false-OK.

### §10.4 What a violating PR looks like (REJECT)

- Adds a SECOND Dhan minter anywhere (that is the defect, from either side).
- Widens the write scope beyond the single `dhan/access-token` ARN, or the read
  scope to a `/dhan/*` wildcard.
- Logs the token, PIN or TOTP; or renders a `reqwest::Error` verbatim (its
  `Display` carries the PIN-bearing query string).
- Publishes a token without the JWT shape check — that check is what stops an
  error body from replacing a working token.
- Makes any failure path silent (returns `Ok` on a failed mint or write).
- Removes the not-invoked alarm, or the `treat_missing_data = "breaching"` on
  it — a dropped schedule is invisible to the Errors alarm.
- Points the schedule at MON-FRI (weekend consumers would read a stale token).
- Deletes §9's publisher without first switching tickvault to READ (that would
  leave the box mintless AND readless).

### §10.5 Honest envelope

> "100% inside the tested envelope, with ratcheted regression coverage: the
> pure logic — SSM path construction, mint-URL building, TOTP generation, JWT
> shape gating, Dhan's 200-with-error envelope, body truncation, and the
> refusal to publish a malformed token — is unit-tested (24 tests) including
> the Dhan/Groww path-collision case and the no-secret-in-error-message case.
> **NOT claimed:** live behaviour. No AWS credentials exist in CI, so the SSM
> reads, the SSM write, the real `generateAccessToken` call, and the
> EventBridge trigger are UNVERIFIED-LIVE until the first scheduled 06:05 IST
> invocation. The real proof is `aws ssm get-parameter --name
> /tickvault/prod/dhan/access-token --query Parameter.LastModifiedDate`
> reading today's date. **NOT claimed:** that this fixes trading — the box
> still cannot start (ap-south-1a capacity; AZ pin at `main.tf:77`), which is
> an independent problem this Lambda deliberately does not touch. **NOT
> claimed:** safe coexistence with a RUNNING box — see §10.3."

### §10.6 Auto-driver explanation

> Sir, our shop cut the Dhan fridge key itself — but only while the shop was
> open. The shop has been shut since Tuesday (no space in the market yard), so
> nobody cut a key, and the key in the shared locker went dead. The neighbour
> kept trying that dead key: 785 tries, 785 failures. Meanwhile the Groww key
> was fresh every morning — because head office cuts THAT one from their own
> office, not from our shop. So now we do the same for Dhan: a small clerk who
> comes at 6:05 every morning, cuts the key, drops it in the locker, and goes
> home. Shop open or shut, the key is always fresh.

### §10.7 Trigger / auto-load

The §7 trigger list applies. Additionally reinforced on any session editing
`crates/aws-lambdas/src/dhan_token_minter.rs`,
`deploy/aws/terraform/dhan-token-minter-lambda.tf`, or any file containing
`dhan-token-minter`, `DHAN_ACCESS_TOKEN_SECRET`, or `generateAccessToken`.
