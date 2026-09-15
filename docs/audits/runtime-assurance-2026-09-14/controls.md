# TickVault phase 2: security and runtime boundary review

Reviewed checkout: `/workspace/scratch/81296451ef3b/tickvault-audit`, commit `6637b52ec8d2604ba06b83b3f898f79aa7a85943`.

Scope: API auth/feed controls; operator and notification Lambdas; selected order-runtime, secret, and deployment boundaries. Initial review was read-only; the coordinator subsequently authorized the six fixes recorded below. No live orders, provider requests, real notifications, production commands, or secret-value inspection occurred. One baseline reproduction executed the exact read-only verification tail with `curl` overridden to fail and `sleep` overridden to return immediately. Rust tests were not executed: `rustc` is unavailable on the default PATH. Findings below refer to the baseline commit and distinguish reproduced behavior from source-traced counterexamples.

## Implementation update

All SEC-01–06 changes have been implemented in the working tree, without publishing or committing. Source line references in the original findings below describe the baseline and have shifted after edits.

| Finding | Implemented behavior | Validation |
|---|---|---|
| SEC-01 | `qc` now queries `/exp` with `COUNT() AS count` and accepts only the exact quoted CSV header plus one canonical unsigned integer row. It preserves trailing newlines, rejects JSON/error/extra-row bodies and transport failure even after a body was returned, caps each request at 5 seconds, and exits nonzero unless every count is explicitly zero. | Exact updated command and fake transport from the Rust regression executed locally: 27/27 fixtures pass. This replaces the first sed-based patch, which peer review showed still accepted malformed/nested JSON. |
| SEC-02 | A present `force` must be a JSON boolean; other types return 400 before any action. Absent/false remains blocked during market hours. | Added mocked dispatch test; Rust execution pending. |
| SEC-03 | TrueData toggle returns 409 before any atomic mutation or persistence in either direction. Error metadata excludes both currently unwired Dhan/TrueData controls from allowed alternatives. | Added all four requested/prior-state combinations; updated authenticated router test to require the intentional 409; added all-current-feed/direction error metadata coverage. Rust execution pending. |
| SEC-04/05 | Delivery cache is staged and committed only after all sends succeed. Delivery requires HTTP 2xx plus a valid top-level JSON object with required boolean `ok: true`; redirects are disabled. Any transport/API/malformed-response failure fails the invocation. | Added failure→retry→successful-dedupe, partial HTTP/transport failure→retry, stale-commit tests, and 17 invalid acknowledgement→retry fixtures. Rust execution pending. |
| SEC-06 | SQL returns confirmed CSV only after command Success with nonempty output. Dispatch error gives 503, failed/cancelled/timed-out/empty results give 502, and unconfirmed deadline gives 504 Pending. Curl is capped at 4 seconds; SSM execution at 10 seconds; delivery window at 30 seconds. | Added mocked status/timeout tests and a fake-curl shell-pipeline failure regression. Rust execution pending. |

Files owned/edited: `crates/api/src/handlers/feeds.rs`, `crates/api/src/lib.rs`, `crates/aws-lambdas/src/operator_control.rs`, `crates/aws-lambdas/src/operator_control_action_commands.rs`, `crates/aws-lambdas/src/telegram_webhook.rs`. Other working-tree changes belong to other agents. `git diff --check` passed for these files.

Exact regression filters (all without real AWS/provider/notification operations):

```text
cargo test -p tickvault-aws-lambdas --lib test_force_requires_json_boolean_before_market_hours_override
cargo test -p tickvault-aws-lambdas --lib test_wipe_verifier_requires_explicit_zero_counts
cargo test -p tickvault-aws-lambdas --lib test_sql_control_
cargo test -p tickvault-aws-lambdas --lib test_failed_ok_is_retried_before_cache_commit
cargo test -p tickvault-aws-lambdas --lib test_partial_delivery_failure_requires_retry_without_cache_commit
cargo test -p tickvault-aws-lambdas --lib test_delivered_cache_preserves_a_newer_observation
cargo test -p tickvault-aws-lambdas --lib test_unacknowledged_telegram_responses_retry_without_cache_commit
cargo test -p tickvault-api --lib test_set_feed_truedata_refused_without_mutating_either_feed
cargo test -p tickvault-api --lib test_feeds_post_with_valid_token_not_401_in_both_modes
cargo test -p tickvault-api --lib test_feed_error_allowed_metadata_excludes_unwired_controls
cargo test -p tickvault-api --lib test_set_feed_unknown_feed_is_rejected_400
```

The SQL filter selects `test_sql_control_requires_confirmed_success_and_remote_deadline`, `test_sql_control_refuses_dispatch_terminal_and_empty_failures`, and `test_sql_control_poll_timeout_is_pending_not_success`.

Wipe fixtures: valid zero CSV with LF, CRLF, or no final newline; positive count; extra nonempty/blank rows; extra field; leading zero; decimal; signed zero; whitespace; exponential notation; incorrect/unquoted header; zero/positive/empty/string/decimal/negative JSON dataset; empty/non-JSON body; peer-reported nested dataset, malformed JSON and explicit error object; a transport error after returning valid zero CSV; and all-request outage. Exactly the three valid-zero cases exit 0 and print `WIPE-COMPLETE`; all 24 failure cases exit 1 and print `WIPE-PARTIAL`. Validation ran only the production read-only verification tail extracted from source and the exact fake `curl`/`sleep` fixture; no destructive command or real HTTP leg ran.

Telegram acknowledgement fixtures: HTTP 199/302/429/500 with `ok: true`; HTTP 200 `ok: false`; empty 200/204; non-JSON/malformed JSON; nested-only `ok`; string/numeric/null `ok`; duplicate `ok`; top-level array-of-object, boolean array, and boolean value. Each must keep the cache empty and permit a subsequent valid 200/`ok: true` recovery message. A typed response parser rejects duplicate `ok`; an explicit object prefix check prevents serde's struct-from-sequence representation from accepting `[true]`. These Rust fixtures have not run.

Remaining practical limits: the batch retry policy is at-least-once and can duplicate already delivered messages; this is deliberate to preserve undelivered alerts. A SQL 504 reports uncertainty and does not claim server-side query cancellation; the remote deadlines bound command/HTTP work after dispatch. The wipe still follows the existing policy that verifies `candles_*` through `candles_1m` alone; this change fixes false completion from missing/invalid counts but does not establish a proof for all dynamically discovered candle tables. Runtime Rust compilation, formatting, full-module regressions, and live deployment remain unverified until the coordinator's AWS gate completes. The conditional subset of local shell fixtures is evidence, not a whole-system guarantee.

## Findings

| ID | Priority | Human-readable failure | Evidence level | Smallest safe next step |
|---|---|---|---|---|
| SEC-01 | High | A database outage is reported as a successful completed wipe. | Exact verification command reproduced with a fake transport. | Missing or invalid counts must fail verification; return nonzero and never print `WIPE-COMPLETE` unless every expected count is explicitly zero. |
| SEC-02 | Medium | Sending `force: "false"` still overrides the market-hours stop/reboot/restart safety gate. | Direct production control-flow trace. | Accept only JSON boolean `true`; reject nonboolean `force`. |
| SEC-03 | Medium | TrueData runtime enable returns success although no lane starts, and its state is absent from response and persistence. | Cross-file production control-flow trace. | Refuse the unwired TrueData control with 409 until runtime control and observable state are implemented. |
| SEC-04 | Medium | A failed recovery notification can be suppressed on retry as though it had been delivered. | Direct failure/retry trace through the shared warm cache. | Commit delivery-deduplication state only after successful delivery. |
| SEC-05 | Medium | A supported batch containing one successful and one failed alert is acknowledged, losing retry of the failed alert. | Direct result predicate/control-flow trace; multi-record live SNS invocation frequency not verified. | Fail on any delivery failure, with per-message success bookkeeping to control duplicates. |
| SEC-06 | Medium | The SQL console returns `ok:true` after SSM failure/timeout, and the remote query may continue after the six-second polling budget. | Direct dispatch/poll/response trace. | Return a typed SSM outcome and 5xx on failure/timeout; enforce remote execution and curl deadlines. |

### SEC-01 — unknown counts become successful wipe verification

File: `crates/aws-lambdas/src/operator_control_action_commands.rs:51`, final element of `WIPE_QUESTDB_COMMANDS`.

The `qc` function pipes `curl -fsS` into two number-extraction `grep` commands. When QuestDB cannot answer or the response does not match, each count is empty. The result line honestly prints `?`, but the completion predicate uses `${T:-0}`, `${D:-0}`, and the equivalent default for all eight counts. Eight unknowns therefore satisfy the all-zero predicate, print `WIPE-COMPLETE`, and exit 0. The outer action has already dispatched destructive cleanup, so this is specifically an assurance failure on an irreversible operation, not merely a cosmetic dashboard issue.

Safe local reproduction extracted only that final read-only raw string. `curl() { return 7; }` and `sleep() { :; }` were exported into a clean Bash invocation. A validation rejected any extracted systemctl/docker/removal/SQL-TRUNCATE command. Actual output:

```text
WIPE-RESULT ticks=? market_depth=? candles_1m=? prev_day_ohlcv=? rest_spot_1m=? rest_option_chain_1m=? rest_option_contract_1m=? rest_fetch_audit=?
WIPE-COMPLETE
```

Exit status: 0. No real HTTP request, file removal, service operation, or wipe occurred.

Regression: run this actual verification tail with fake `curl` for all-eight-zero, one-positive, one-missing, all-missing, malformed JSON, and non-2xx responses. Only explicit eight-zero must complete. Also check its exit status: the present nonzero-count `WIPE-PARTIAL` branch itself ends in an `echo`, hence still exits successfully. Derive the verification set from the same target policy where possible; the truncation target selector covers all `candles_*`, whereas verification currently checks only `candles_1m`.

### SEC-02 — permissive truthiness defeats explicit force semantics

File: `crates/aws-lambdas/src/operator_control.rs`.

`truthy` at 379–387 treats any nonempty string, array, or object and any nonzero number as true. `route` at 1332 applies it directly to `payload.force`. The market-hours lifecycle guard at 1347 tests `!force` before dispatching `stop`, `reboot`, `restart-app`, or `stop-app`.

Counterexample: an authenticated `{"action":"stop","force":"false"}` during market hours bypasses the guard and reaches `ec2_stop`. The API tells the caller that `{"force":true}` is required, but the value `"false"` is accepted as an override. The data-destructive hard lock at 1337 remains effective during the ordinary weekday window; this finding does not claim its bypass.

Minimal fix: use `payload.get("force").and_then(Value::as_bool).unwrap_or(false)`, or preferably return 400 for a present nonboolean field. Mock-shell regression matrix: absent, null, false, true, `"false"`, `"true"`, 0, 1, [], [false], {}, and {"x":false}; only literal true may invoke the lifecycle action during the protected window. No real EC2 actions are necessary.

### SEC-03 — automatic enum inclusion exposes an unwired control

Evidence chain:

* `crates/common/src/feed.rs:44,76–87`: `ALL`, `parse`, and `is_runtime_toggleable` include TrueData.
* `crates/api/src/handlers/feeds.rs:97–264`: the authenticated route accepts TrueData; both 409 guard branches are specifically `feed == Feed::Dhan`; execution reaches `set_enabled`, persistence, and HTTP success.
* `crates/api/src/feed_state.rs:143–148,274–279`: the TrueData atomic is stored, but `truedata_lane_running` starts false. The application has no TrueData lane spawn. Its only `truedata` references in `main.rs` are replay accounting.
* `crates/api/src/feed_state.rs:39–48,284–289`: `FeedStatus` and `snapshot()` contain only Dhan.
* `crates/api/src/handlers/feeds.rs:35–39,80–85`: success response contains only Dhan.
* `crates/api/src/feed_state_persist.rs:57–60,174–176`: persistence contains only Dhan.

Counterexample: authenticated TrueData enable returns 200, with a Dhan-only response, no running TrueData lane, and no durable TrueData choice. This is a counterexample to the claimed common runtime behavior. It is not an unauthenticated mutation bypass.

Minimal fix: make unwired feed control explicitly unavailable, with a 409 reason, before any state mutation or disk write. Router regression should call the real authenticated route, assert 409, assert no atomic change, and assert no persistence. A future real implementation needs registry-derived status/persistence and an acknowledged lane transition; merely adding another response boolean would not start a lane.

### SEC-04 — delivery cache records attempted recovery messages as delivered

File: `crates/aws-lambdas/src/telegram_webhook.rs`.

The production `handle` folds records into the shared `LAST_SENT` cache at 1379–1382, then sends at 1387. `fold_records` writes an OK entry at 1121 before the transport runs. `should_suppress_ok` at 959–964 suppresses a subsequent OK in the suppression window. No failure path rolls back that cache entry. `nothing_was_delivered` requires at least one failure, so a retry producing zero texts and zero failures is accepted.

Counterexample: single lone-OK record → HTTP/transport failure → invocation error → warm retry before the suppression window expires → fold emits no message → `{sent:0, failures:[]}` → invocation returns success. This affects recovery notices; ALARM records themselves are not suppressed by this cache, and the finding does not claim otherwise.

Regression: call the production `deliver` seam twice with the same cache and record, injected first-send failure and second-send success. The second transport must actually be called. Commit cache updates only for messages acknowledged by the transport, including folded multi-alarm recovery messages.

### SEC-05 — partial notification failure is accepted

Same file, `send_texts` 1238–1266; `nothing_was_delivered` 1281–1287; `handle` 1402–1415.

Counterexample supported by the exposed batch-processing contract: two distinct ALARM texts; first returns 200, second returns error. Result is `{sent:1, failures:[...], records:2}`. The guard is false because `sent != 0`; `handle` returns `Ok`, acknowledging the batch while the second alert was not delivered. Actual SNS payloads usually need independent validation before claiming this exact multi-record input occurs in normal production. The API and tests intentionally support multiple records, so this remains an extreme-input counterexample.

Minimal regression uses only the injected sender and asserts the top-level acknowledgement decision, not just the `failures` vector. Correcting the predicate to any failure is necessary but should be coordinated with SEC-04 and per-message delivered state to avoid turning retries into misleading duplicate episode counts.

### SEC-06 — SQL timeout is treated as successful empty results

File: `crates/aws-lambdas/src/operator_control.rs`.

The SQL action at 1611–1633 calls `ssm_shell_sync` and unconditionally responds HTTP 200, `{ok:true, csv:out}`. `ssm_shell_sync` at 1968–1995 returns an empty String for send failure or poll-budget expiry; terminal Failed/Cancelled/TimedOut states are flattened into stdout/stderr. The SQL curl at 1628 has neither `--max-time` nor a connection deadline; `ssm_shell` at 1952–1961 supplies commands only, without an explicit execution timeout. Expiring the Lambda's six-second polling budget does not cancel its remote command.

Consequences: a stopped/offline SSM agent, denied RunCommand, or slow query can produce `ok:true,csv:""`; a slow remote command can outlive the visible operation. This differs from the QuestDB console proxy, which has typed transport errors, response caps, and a 504 mapping.

Regression: mock SSM send failure, terminal failure, deadline expiry, and genuine successful CSV output; failure/unknown must not return `ok:true`. Add request and remote-execution deadlines sized consistently with the caller, and explicit cancellation/status handling for commands that have already been dispatched.

## Positive findings and claim boundaries

* API POST feed controls and debug routes are structurally covered by bearer middleware (`crates/api/src/lib.rs:159–209`). The normal boot resolves an SSM token and constructs `ApiAuthConfig::from_token` (`crates/app/src/main.rs:3980–4036`). Empty-secret disabled auth remains a library/testing construction; no unauthenticated production bypass was established.
* Dhan's runtime-control refusal is now explicit for both directions (`handlers/feeds.rs:138–188`). Actual boot configuration plus restart still governs the lane. This is safe refusal, not general dynamic runtime reconfiguration.
* Operator Lambda auth rejects an empty secret and compares bearer bytes with `aws_lc_rs::constant_time` (`operator_control.rs:182–197`). Secret loading fails closed when its short cache cannot refresh. No actual secret values were inspected.
* Telegram transport-error logging explicitly uses `without_url()` at 1394–1397, avoiding the URL-carried bot-token error leak in that path. A naive finding based only on the lower transport function would have been incorrect.
* OMS token bridging rejects absent/expired tokens and the shared order HTTP builder disables redirects (`crates/app/src/oms_wiring.rs`). The reviewed order runtime constructs an OMS whose default is paper mode; this does not prove real trading readiness or all order state transitions.
* The application/API/Lambda implementations reviewed are Rust. The deployed runtime boundary is not solely Rust: `WIPE_QUESTDB_COMMANDS` and SQL/feed controls dispatch shell, `deploy/systemd/tickvault.service:106` executes Bash during startup, and Docker Compose runs QuestDB, Loki, and Alloy. Embedding shell inside Rust strings does not make the executed logic Rust. Runtime loops scan rows/files/messages and remote commands perform I/O; no whole-workspace O(1) proof follows.
* Public DB-backed endpoints share a global 5 requests/sec, burst-10 limiter (`crates/api/src/public_guard.rs:42–45,66–90`, router:284–298). That is a protection for the current operator surface, not evidence of support for millions of independent customers.
* This review did not validate production IAM state, live deployed binaries, network topology, filesystem durability under actual power loss, exchange behavior, live special trading sessions, or all shutdown interleavings. Source-traced failures are sufficient to reject an unconditional guarantee, while passing local cases cannot establish one.

## Recommended priority

First fix SEC-01 and add fake-transport tests around the actual destructive-operation verification path. Then refuse SEC-03's unwired control and enforce SEC-02's typed safety input. Correct delivery acknowledgement and cache ordering together for SEC-04/05. Finally replace the SQL action's empty-string error collapse and add consistent execution deadlines. No production action is needed to make any of these corrections reviewable.
