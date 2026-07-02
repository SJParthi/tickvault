# Implementation Plan: CloudWatch EMF source_labels fix + liveness metric-filter fallback

**Status:** APPROVED
**Date:** 2026-07-02
**Approved by:** Parthiban (operator) — standing directive this session: "go ahead with the plan always; everything comprehensively automated"

> Guarantee matrices: this plan cross-references the mandatory 15-row + 7-row
> matrices in `.claude/rules/project/per-wave-guarantee-matrix.md` and the
> Zero-Loss Guarantee Charter §3 check (carried in the PR body). Scope is
> observability-config + ratchet-test only — no hot path, no tick path, no
> order path, no new pub fn.

## Design

**Root cause (B1 evidence, 2026-07-02, boto3-verified):** the CloudWatch
agent's prometheus `emf_processor.metric_declaration` in BOTH
`deploy/aws/cloudwatch-agent.json` (the copy `deploy-aws.yml` installs on
every deploy) and `deploy/aws/terraform/user-data.sh.tftpl` (fresh-instance
bootstrap) used `source_labels: ["__name__"]` with the 19-name metric regex
as `label_matcher`. `__name__` is not a label on the scraped series at the
emf_processor stage (live `/tickvault/prod/metrics` events carry only
`host`/`instance`/`job`/`prom_metric_type`), so the declaration matched ZERO
metrics → no `_aws` EMF envelope → `Tickvault/Prod` namespace empty ~40 days
→ `tv-prod-boot-heartbeat-missing` + `tv-prod-market-hours-liveness-missing`
alarms permanently blind on missing data.

**The fix (3 legs):**
1. Both agent configs: `source_labels: ["host"]`, `label_matcher:
   "^tickvault-prod$"` (the literal static label prometheus.yaml stamps),
   `metric_selectors` alone filters metric names. `metric_selectors`
   additionally grows 19 → 21 names (`tv_process_rss_bytes`,
   `tv_subsystem_memory_estimated_bytes`) so tomorrow's 2K-universe memory
   measurement reads real CloudWatch metrics instead of Logs-Insights parses.
2. Belt-and-suspenders: two terraform `aws_cloudwatch_log_metric_filter`
   resources (`deploy/aws/terraform/metrics-log-metric-filters.tf`) extract
   `tv_boot_completed` + `tv_realtime_guarantee_score` from the CURRENT
   plain-JSON log events (`{ $.tv_boot_completed = * }`, value
   `$.tv_boot_completed`) into `Tickvault/Prod` with the `host` dimension
   extracted from `$.host` — exactly matching the alarms' `dimensions
   {host=tickvault-prod}`. This guarantees the two alarms go green even if
   the EMF theory is imperfect on the live agent build.
3. Drift-guard tests in `crates/common/tests/cloudwatch_app_alarms_wiring.rs`
   (tickvault-common) updated: the retired label_matcher==metric_selectors
   pin is replaced by a root-cause pin (source_labels ["host"], matcher
   `^tickvault-prod$`, `__name__` banned), a 21-name metric_selectors count
   pin, and a fallback-filter coverage pin. The two-config-identical
   invariant (`test_reference_cloudwatch_agent_json_matches_deployed_emf_declaration`)
   is preserved unchanged.

## Edge Cases

- **Dimension match:** metric filters publish dimensionless metrics by
  default — invisible to alarms keyed on `host`. Solved via dimension
  extraction `host = "$.host"` from the JSON events (verified present in the
  live event sample). Pinned by the new test.
- **Missing-data semantics:** the alarms' entire detection model is
  `treat_missing_data = "breaching"`. The filters deliberately set NO
  `default_value` so a dead/stopped app still produces MISSING data.
- **Labeled gauge collapse:** `tv_subsystem_memory_estimated_bytes{component}`
  publishes under the single `[["host"]]` dimension set — CloudWatch merges
  components into one statistic set (Sum = total estimated bytes, Max =
  largest component). Acceptable for the RAM measurement; per-component
  breakdown remains available via Logs Insights. Honestly noted, not hidden.
- **Log group not terraform-managed:** `/tickvault/prod/metrics` is created
  by the agent; the filters reference it by name. It exists live (events
  since ~May), so apply cannot fail on a missing group.
- **Double publish:** once EMF works, both EMF and the filters publish the
  same metric name+dimensions — values merge into the same series
  (identical samples), no alarm impact.
- **tftpl escaping:** the edited block contains no new `${}` sequences, so no
  templatefile interpolation change.

## Failure Modes

- **EC2 instance replacement via user_data change:** `main.tf` sets
  `user_data_replace_on_change = false` AND lists `user_data` (and
  `instance_type`) in `lifecycle.ignore_changes` — the tftpl edit produces NO
  instance diff. HARD GATE regardless: the PR's CI Terraform-plan output is
  READ before merge; if it shows the instance/EIP/EBS being replaced or
  destroyed, the tftpl change is dropped from the PR and deferred to a
  manual window.
- **EMF fix still wrong on the live agent build:** the metric filters are
  agent-independent (they read the log events that already flow), so the two
  liveness alarms still recover. The EMF leg is explicitly validate-on-boot
  (B1 Q4 verification commands) — never claimed verified pre-boot.
- **Filter pattern mismatch:** pinned by the new ratchet test against the
  exact JSON field names observed in the live event sample.
- **Agent rejects new config:** deploy-aws.yml runs `fetch-config` on every
  deploy; a rejected config leaves the previous one running (same blind
  state as today — no regression) and the filters still cover the alarms.

## Test Plan

- `cargo test -p tickvault-common --test cloudwatch_app_alarms_wiring` —
  all drift-guard tests green, including the three new/replaced pins:
  `test_deployed_emf_source_labels_match_a_real_series_label`,
  `test_emf_metric_selectors_name_count_is_twenty_one`,
  `test_log_metric_filter_fallback_covers_both_liveness_alarm_metrics`.
- `python3 -m json.tool deploy/aws/cloudwatch-agent.json` — JSON parses.
- CI `Terraform plan` job — read output: 2 metric-filter ADDs, NO
  instance/EIP/EBS replace/destroy.
- Post-merge (tomorrow 08:45 IST boot): B1 Q4 command list — `list-metrics`
  non-empty, `_aws` in events, both alarms → OK; read-only boto3 verify the
  two metric filters exist on the log group.

## Rollback

- `git revert` of the single squash commit restores the previous (broken but
  known) config; the next deploy's `fetch-config` reinstalls it.
- The metric filters are pure additive terraform resources — `terraform
  destroy -target` or deleting `metrics-log-metric-filters.tf` removes them
  with zero blast radius (no other resource references them).
- No schema, no data, no hot-path change — nothing to migrate back.

## Observability

- This change IS the observability fix: it restores the `Tickvault/Prod`
  custom-metric namespace feeding all 17 app alarms + the two gated liveness
  alarms, and adds 2 memory gauges for the 2K-universe measurement.
- Verification signals: `aws cloudwatch list-metrics --namespace
  Tickvault/Prod` non-empty; `{ $._aws EXISTS }` filter-log-events non-empty;
  `tv-prod-boot-heartbeat-missing` + `tv-prod-market-hours-liveness-missing`
  transition ALARM → OK in alarm history after the 08:45 IST boot.
- Regression protection: the updated ratchet tests in
  `crates/common/tests/cloudwatch_app_alarms_wiring.rs` fail the build if the
  `__name__` shape returns, the two configs drift, a liveness metric leaves
  the filter list, or a fallback filter loses its host dimension.

## Plan Items

- [x] Fix EMF declaration shape + 21-name selectors in both agent configs
  - Files: deploy/aws/cloudwatch-agent.json, deploy/aws/terraform/user-data.sh.tftpl
  - Tests: test_deployed_emf_source_labels_match_a_real_series_label, test_emf_metric_selectors_name_count_is_twenty_one, test_reference_cloudwatch_agent_json_matches_deployed_emf_declaration
- [x] Add log-metric-filter fallback for the two liveness alarm metrics
  - Files: deploy/aws/terraform/metrics-log-metric-filters.tf
  - Tests: test_log_metric_filter_fallback_covers_both_liveness_alarm_metrics
- [x] Update drift-guard ratchets to pin the new correct shape
  - Files: crates/common/tests/cloudwatch_app_alarms_wiring.rs
  - Tests: (the file itself — full test binary green)
