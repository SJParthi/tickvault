# Lambda account concurrency quota raise — 2026-07-14 incident remediation.
#
# THE INCIDENT: the QuestDB console gateway (tv-prod-questdb-console,
# questdb-console.tf) returned 76 intermittent 5xx (503) between
# 13:51-13:52 IST on 2026-07-14. CloudWatch showed the gateway 5xx series
# EXACTLY equal to the console FRONT Lambda's Throttles series, and
# `aws lambda get-account-settings` returned ConcurrentExecutions: 10 —
# this account's ap-south-1 Lambda concurrency ceiling is 10, not the
# 1000 AWS grants by default to established accounts. Each console request
# holds 2 of the 10 slots (the front Lambda synchronously invokes the VPC
# back relay), so the console SPA's ~11 req/s burst pegged the ceiling and
# API Gateway surfaced every throttled invoke as a 503. QuestDB itself was
# healthy throughout (zero Lambda errors, zero backend connect failures).
#
# WHY 1000: it is AWS's normal account default for this quota; raises back
# to the default are typically auto-approved without human review.
#
# WHY NOT RESERVED CONCURRENCY: structurally unavailable at a 10-slot cap.
# AWS requires >= 100 UNRESERVED concurrency to remain after any per-
# function reservation, so no reservation is settable until this raise
# lands — reserving is a possible follow-up AFTER the quota is 1000, not
# an alternative to it.
#
# SHARED-POOL RISK THIS REMOVES: at a 10-slot ceiling, a console burst
# pegging all 10 slots also throttles EVERY other Lambda in the account —
# including the SNS→Telegram pager webhook (async invoke, so throttles
# mean DELAYED pages via the retry queue, not dropped ones — but a delayed
# pager during an incident is exactly the wrong failure mode). At 1000 the
# console and the alerting path can no longer starve each other.
#
# REGION SCOPE: Service Quotas are per-region; this resource uses the
# provider's default region — ap-south-1 per the provider block in
# versions.tf (var.aws_region, validation-locked to ap-south-1 in
# variables.tf) — which is the region where the 10-slot cap was observed.
#
# NOT gated on var.enable_questdb_console: the raise benefits every Lambda
# in the account (pager webhook, operator control, deploy watchdog, budget
# killswitch, ...), not just the console chain.
#
# HONEST ENVELOPE: terraform SUBMITS the quota-increase request; APPROVAL
# is AWS-side and asynchronous. While the request is pending,
# `terraform plan` may show the requested value (1000) against the still-
# effective 10 — that is expected pending-request behaviour, not drift.
# COST: $0 — Service Quotas requests are free and a higher concurrency
# ceiling carries no standing charge; Lambda still bills per-invocation
# exactly as today.
#
# PROVIDER FACTS (verified 2026-07-14 against hashicorp/aws v5.80.0 docs +
# source, internal/service/servicequotas/service_quota.go):
#   - quota_code / service_code / value are the only arguments (all
#     required) — schema matches the workspace pin ~> 5.80 (versions.tf).
#   - While the increase request is Pending/CaseOpened, the provider READS
#     the REQUESTED value, so subsequent plans show no diff and no error.
#   - A Denied/CaseClosed request re-surfaces as drift (value 10 vs 1000)
#     and the next apply re-submits; if AWS approves a DIFFERENT value,
#     same drift-and-resubmit behaviour.
#   - `terraform destroy` is a provider NO-OP (DeleteWithoutTimeout =
#     schema.NoopContext) — quotas cannot be lowered; the resource just
#     leaves state.
#
# QUOTA FACTS (verified LIVE 2026-07-14, ap-south-1, get-service-quota):
# L-B99A9384 = "Concurrent executions" (lambda), Adjustable = true,
# applied value = 10, AWS default = 1000, and NO open increase request
# exists (a pre-existing open request would make the provider's
# RequestServiceQuotaIncrease collide with ResourceAlreadyExists; none).
#
# CI IAM (verified LIVE 2026-07-14): terraform-apply.yml's ACTIVE
# principal is the AdministratorAccess bootstrap identity (IAM user
# dlt-admin, active key last used 2026-07-14; the tv-prod-github-deploy
# OIDC role shows RoleLastUsed = never and carries only the narrow
# deploy-aws inline policy), so the servicequotas actions this resource
# needs — GetServiceQuota, RequestServiceQuotaIncrease,
# ListRequestedServiceQuotaChangeHistoryByQuota — are covered today. IF
# the account ever completes the BOOTSTRAP-ONE-TIME.md step-5 migration to
# the least-privilege OIDC role, that role's policy (oidc.tf) must gain
# those three actions — alongside the far larger terraform-wide grant that
# migration already requires (the narrow policy cannot run terraform at
# all; pre-existing gap, not introduced here).

resource "aws_servicequotas_service_quota" "lambda_concurrent_executions" {
  service_code = "lambda"
  quota_code   = "L-B99A9384" # Concurrent executions
  value        = 1000
}
