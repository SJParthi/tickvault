# Implementation Plan: Operator Portal REST-Only Cleanup

**Status:** VERIFIED
**Date:** 2026-07-16
**Approved by:** Parthiban (operator, 2026-07-16) — quote: "since we have deleted and removed everything do we really need all these displaying and views in dashboard — it can be removed?" → "approved"

> Guarantee matrices: cross-reference `.claude/rules/project/per-wave-guarantee-matrix.md`
> (15-row + 7-row). This is a deploy/-only Python Lambda + HTML change — no Rust,
> no hot path, no new tick-drop path, no DEDUP-key change, no new pub fn. The
> per-item proof is the pytest suite in the same directory (lockstep tests per
> panel) + the untouched `deploy_provenance_guard` Rust ratchet re-run.

## Design

The runtime is REST-only (Dhan live WS retired 2026-07-13, Groww live WS retired
2026-07-15 — `websocket-connection-scope-lock.md` amendments). The operator portal
(`deploy/aws/lambda/operator-control/handler.py`) still renders live-feed-era
panels whose producers are deleted. This change:

**REMOVE (producer dead):** peak-ticks/sec pill + live ticks/sec sparkline;
guarantees shields "Sub-second fix", "Tick conservation" (whole classifier:
`_CONSERVATION_ENVELOPE`, `_parse_conserve_rows`, `_classify_tick_conservation`,
CONSERVE + WS_DISC view commands, `ws_disconnects_today`), "Peak ticks/second";
the latency card's per-feed WS TCP/TLS probe table (`_FEED_LIVE_HOSTS` — both
probed hosts are retired edges) + the exchange→received lag-percentile grid over
`ticks` (all `_pctl_*` machinery) + the tick-path histogram shields
(tick-processing / wire→done / tick-flush windowed percentile machinery);
the Data-tab cross-verify card (`_CROSS_VERIFY_COMMANDS`, `_parse_cross_verify`,
action `cross_verify`, markup + `loadCrossVerify`); the admin "Wipe GROWW only"
control (`_GROWW_WIPE_PY`, `_wipe_groww_commands`, action `wipe-groww`, radio +
`wipeGroww` JS + pollNuke branches; `_DESTRUCTIVE`/`_DATA_DESTRUCTIVE` shrink).

**REPOINT (concept kept, REST-era source):** overview hero → today's row counts
of `spot_1m_rest` + `option_chain_1m` + `option_contract_1m_rest` (total + per-
table + per-feed split, plain-English labels); dedup shield → 4 upsert-key
columns of `spot_1m_rest` (ts, security_id, exchange_segment, feed per
`DEDUP_KEY_SPOT_1M_REST`); feeds card → keep toggles/ENABLED, drop dead
ticks/subscribed counters, add a `rest_fetch_audit`-sourced per-feed line
(today ok/failed/rate-limited + last-hour p50/p99 of `close_to_data_ms` for
outcome='ok'); Data-tab bars → "official minute rows captured today" per
table per feed; latency card → keep QuestDB RTT + clock skew + dormant Order
placement shield, add today's per-(feed, leg) p50/p99 of `close_to_data_ms`
via `approx_percentile` ("how fast after each minute closed" — the 15:45
scorecard digest language); Wipe-ALL truncate list gains the four live tables
(`spot_1m_rest`, `option_chain_1m`, `option_contract_1m_rest`,
`rest_fetch_audit`) — destructive-surface change, called out in the commit
body; DB console default query → `SELECT * FROM spot_1m_rest ORDER BY ts DESC
LIMIT 50`.

**KEEP untouched:** instance pills + start/stop, app pill, market pill, AWS
strip, DB console + SQL gate, QuestDB console button, admin app-control,
docker-reset/nuke + command-status polling, device lock, `_provenance_line`
footer (guarded by `crates/common/tests/deploy_provenance_guard.rs`), the
`logs` action (tickvault-logs MCP server caller), `recent_errors`.

## Edge Cases

- Box stopped / QuestDB down: every new field degrades to blank/empty (the
  existing labeled-line convention) — the UI renders "—"/"no pulls recorded",
  never fabricated zeros (audit Rule 11).
- A REST table absent on a fresh box: `curl -f` fails → empty value → honest
  blank; the wipe's dynamic `tables()` discovery only truncates tables that
  exist.
- `close_to_data_ms = -1` sentinel rows (not measured / backfilled): filtered
  by `close_to_data_ms >= 0`, which also satisfies `approx_percentile`'s
  non-negative-input requirement.
- IST timebase: `rest_fetch_audit.ts` is IST-shifted while QuestDB `now()` is
  UTC — the last-hour predicate uses `dateadd('m', 330, now())` (the 2026-07-07
  pctl lesson); today windows keep the house `ts IN today()` convention used by
  every existing view command.
- Feed/leg tokens read back from the DB are charset-validated
  (`[a-z0-9_-]{1,32}`) before rendering; all cells render through `esc()`.
- Empty `rest_fetch_audit` per-feed slot: the feeds card says "no official-
  candle pulls recorded today" instead of 0s.
- wipe-groww / cross_verify POSTs after removal: fall through to the unknown-
  action 400 (no silent success).

## Failure Modes

- SSM offline (box stopped): `_ssm_shell_sync` returns "" — parsers yield the
  blank/empty structures; view still returns 200 with `instance_state`.
- QuestDB reachable but a query rejected (schema drift): that labeled value is
  empty → the specific panel reads "—"/unreachable; nothing else breaks.
- Wipe-ALL now truncates the live REST tables: mid-market execution stays
  impossible (the `_DATA_DESTRUCTIVE` market-hours hard lock is unchanged);
  the honest WIPE-COMPLETE marker now also requires `spot_1m_rest == 0`.
- The latency measure is cheaper (no 25s probe fan-out) — worst case now
  ~12s; `_LATENCY_TIMEOUT_SECS` reduced with margin (15s), each curl
  `--max-time`-bounded, so a dead endpoint can never hang the Lambda.
- Provenance footer untouched — a footer failure still serves the page
  (existing fail-soft).

## Test Plan

`deploy/aws/lambda/operator-control/test_handler.py`, same commit as the code:

- REMOVE tests pinning dead panels: TickConservationShield, ParseCrossVerify,
  CrossVerifyCard, WipeGrowwGate, HtmlWipeGrowwButton, probe/pctl/windowed
  Latency tests, sparkline/tps assertions.
- UPDATE: ParseView (new REST-era fields), ViewCommands (dedup query targets
  `table_columns('spot_1m_rest')` with `%3D`), FeedCounts (SPOT_BY_FEED),
  FeedsView (+rest_audit/rest_lat_hour parsing), DedupFiveColumnCheck → 4-key
  spot_1m_rest shield, FeedsCardHtml (no ticks/subscribed; rest-pull line),
  LatencyCardHtml (latrest table, no latfeeds/latpctl, box-wide labels kept),
  RedesignThreeTabs (3-wipe danger zone), DataDestructiveMarketHoursLock
  (3-action confirm map), WipeGate (+live-table truncate assertions).
- ADD: view SQL targets the three live tables and none of the dead ones;
  `cross_verify` + `wipe-groww` actions return 400 unknown; wipe-all target
  list includes the four live tables + spot verification; DB console default
  query pinned; `LegacyLiveFeedPanelsRemoved` (helpers + markup absence);
  rest-latency command/parser tests.
- KEEP: test_logs_route_kept_for_mcp_server, DeployProvenance class,
  three-tabs test, SQL gate tests, docker-reset/bare-nuke tests.
- Run: `python3 -m pytest test_handler.py -q` → ALL green.
- Rust: `cargo test -p tickvault-common --test deploy_provenance_guard` (the
  guard scans handler.py; must stay green).

## Rollback

Single-file-pair change (`handler.py` + `test_handler.py`) + this plan file.
`git revert` of the squash commit restores the previous portal verbatim; the
Lambda redeploys from the repo tree on the next terraform apply (portal sha is
CI-injected via `TF_VAR_portal_git_sha`), so rollback = revert + apply. No
schema, no data migration, no Rust surface — nothing else to unwind.

## Observability

- The portal IS the observability surface being corrected: dead panels showed
  frozen/false data (e.g. the cross-verify card frozen at a 2026-07-13 FAIL).
- New panels source exclusively from LIVE producers: `spot_1m_rest`,
  `option_chain_1m`, `option_contract_1m_rest`, `rest_fetch_audit` (written by
  all four feed/leg pairs per `rest-1m-pipeline-error-codes.md` §2 GAP-11).
- Honest degrade text everywhere ("no pulls recorded today", "—") — never a
  fabricated zero or green (audit Rule 11).
- Lambda-side failures keep the existing print→CloudWatch-log trail; no new
  ErrorCode (no Rust emit site changes in this PR).
- Follow-ups recorded for the PR body: `GET /api/debug/cross-verify/latest`
  becomes caller-less (crates/api debug handler — separate PR); the dead
  tick-path metrics in core/storage retire with the committed C-phase candle
  machinery retirement (not this PR).

## Plan Items

- [x] Item 1 — Plan file (this file)
  - Files: .claude/plans/active-plan-portal-cleanup.md
  - Tests: n/a (docs)
- [x] Item 2 — handler.py: remove dead panels (sparkline, tps pill, conservation
  classifier, sub-second/peak shields, WS probe table + lag grid + tick-path
  histogram machinery, cross-verify card+action, wipe-groww control+action)
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: test_backend_helpers_gone, test_html_panels_gone,
    test_cross_verify_action_removed, test_wipe_groww_action_removed
- [x] Item 3 — handler.py: repoint hero/bars/dedup shield/feeds card/latency
  card to spot_1m_rest / option_chain_1m / option_contract_1m_rest /
  rest_fetch_audit; wipe-ALL truncate list + verification extended; DB console
  default query
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: test_parses_labeled_snapshot, test_view_commands_target_live_rest_tables,
    test_dedup_key_query_targets_spot_1m_rest, test_parse_feeds_view_rest_lane_fields,
    test_feeds_card_shows_rest_pull_line_not_tick_counters,
    test_latency_commands_rest_era_bounded, test_parse_latency_full,
    test_wipe_questdb_truncates_live_rest_tables_too,
    test_db_console_default_query_targets_live_table
- [x] Item 4 — test_handler.py lockstep: remove/update/add per the Test Plan;
  full suite green
  - Files: deploy/aws/lambda/operator-control/test_handler.py
  - Tests: the whole suite (`python3 -m pytest test_handler.py -q`)
- [x] Item 5 — verify guards: plan-gate, plan-verify, deploy_provenance_guard
  - Files: n/a (verification)
  - Tests: cargo test -p tickvault-common --test deploy_provenance_guard

- [x] Item 6 — Review round 1 fixes (3-reviewer consolidated findings,
  2026-07-16): M1 feeds-view SSM budget 20s → 28s (> the 24s worst-case
  `--max-time` sum, drift-proof test parses the commands); M2 wipe verify
  gate rebuilt — all FOUR live REST tables verified, missing/erroring counts
  default to 0 (absent = wiped; the `:-1` default made every post-nuke wipe
  read WIPE-PARTIAL forever), TRUNCATE-FAILED stays the loud failure path;
  M3 feed-toggle confirm dialog reworded to REST-lane truth (official-candle
  pulls, no "live feed"/"Ticks"); M4 honest hero + bars — '—' instead of an
  animated fabricated 0 when counts are unreadable; M5 `_FEEDS_VIEW_COMMANDS`
  pinned (rest_fetch_audit today-window, IST `dateadd('m', 330, now())`
  timebase, outcome='ok', `close_to_data_ms >= 0` sentinel exclusion); M6
  README rewritten for the REST-only portal (150 tests); L1 comment-stripped
  retired-WS-host resurrection scan; L2 dead artifacts removed (td.win CSS,
  shield() tip param, latrestnote); L3 stale comments reworded (cross-verify
  card reference, wipe-groww test comment); L4 feeds-card "failed" bucket
  excludes skipped/boundary_skipped (never-attempted minutes); L5 confirm-map
  literal corrected to NUKE-DOCKER.
  - Files: deploy/aws/lambda/operator-control/handler.py,
    deploy/aws/lambda/operator-control/test_handler.py,
    deploy/aws/lambda/operator-control/README.md
  - Tests: test_feeds_timeout_budget_exceeds_curl_max_time_sum,
    test_wipe_questdb_truncates_live_rest_tables_too,
    test_disable_asks_for_confirmation,
    test_hero_never_renders_fabricated_zero,
    test_bars_render_dash_for_unreadable_counts,
    test_rest_audit_curl_targets_todays_fetch_log,
    test_rest_lat_hour_curl_ist_timebase_ok_filter_and_sentinel,
    test_retired_ws_probe_hosts_absent_from_code,
    test_failed_bucket_excludes_never_attempted_minutes

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Box running, REST legs pulling | hero shows total official-candle rows today; splits per table + feed |
| 2 | Box stopped | blank snapshot, greyed dedup shield, honest banners — no 500 |
| 3 | rest_fetch_audit empty (fresh box) | feeds card "no pulls recorded today"; latency table "-" |
| 4 | POST {"action":"cross_verify"} | 400 unknown action |
| 5 | POST {"action":"wipe-groww"} | 400 unknown action |
| 6 | Forced off-hours wipe-ALL | truncates ticks + candles_* + prev_day_ohlcv + the 4 live REST tables; WIPE-COMPLETE requires spot_1m_rest == 0 too |
| 7 | MCP server POST {"action":"logs"} | 200 with raw journal output (unchanged) |
| 8 | GET / | page renders with provenance footer (unchanged literals) |
