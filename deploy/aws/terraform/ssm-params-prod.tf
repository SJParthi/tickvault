# =============================================================================
# Production SSM Parameter SHELLS — production cutover (2026-06-30, data-only)
# =============================================================================
# Terraform creates the SecureString parameter SHELLS so the IAM policy + the
# app's resolve_environment() prefix (/tickvault/${var.environment}/*, default
# `prod`) line up. The OPERATOR populates the real secret VALUES out-of-band
# (AWS Console / `aws ssm put-parameter --overwrite`) — secrets NEVER live in
# this repo or in terraform state as real values.
#
# Mechanism (verified against crates/core/src/auth/secret_manager.rs +
# crates/common/src/constants.rs — the app reads these exact paths):
#   /tickvault/<env>/dhan/client-id        (SSM_DHAN_SERVICE / DHAN_CLIENT_ID_SECRET)
#   /tickvault/<env>/dhan/client-secret    (DHAN_CLIENT_SECRET_SECRET)
#   /tickvault/<env>/dhan/totp-secret      (DHAN_TOTP_SECRET)
#   /tickvault/<env>/api/bearer-token      (SSM_API_SERVICE / API_BEARER_TOKEN_SECRET)
#   /tickvault/<env>/questdb/pg-user       (SSM_QUESTDB_SERVICE / QUESTDB_PG_USER_SECRET)
#   /tickvault/<env>/questdb/pg-password   (QUESTDB_PG_PASSWORD_SECRET)
#   /tickvault/<env>/telegram/bot-token    (SSM_TELEGRAM_SERVICE / TELEGRAM_BOT_TOKEN_SECRET)
#   /tickvault/<env>/telegram/chat-id      (TELEGRAM_CHAT_ID_SECRET)
#   /tickvault/<env>/network/static-ip     (SSM_NETWORK_SERVICE / STATIC_IP_SECRET)
#   /tickvault/<env>/operator/github-token (operator triage GitHub token)
#
# With var.environment = "prod" these resolve to /tickvault/prod/*, matching the
# app box's systemd `TV_ENVIRONMENT=prod`.
#
# WHY `lifecycle { ignore_changes = [value] }`:
#   The placeholder below seeds the shell on first `apply`. Once the operator
#   overwrites the real secret value, terraform MUST NOT revert it back to the
#   placeholder on the next `apply`. ignore_changes=[value] makes terraform own
#   the param's existence + type, while the operator owns its value. This is the
#   standard "terraform creates the shell, human fills the secret" pattern and
#   keeps real secrets out of state.
#
# OPERATOR ONE-TIME STEP (per parameter, after `terraform apply`):
#   aws ssm put-parameter --region ap-south-1 \
#     --name /tickvault/prod/dhan/client-id \
#     --type SecureString --value '<REAL VALUE>' --overwrite
# =============================================================================

# Placeholder seeded into every shell on first apply. The operator overwrites
# each with the real secret value out-of-band; ignore_changes keeps it that way.
variable "ssm_placeholder_value" {
  description = "Placeholder seeded into every prod SSM SecureString shell on first apply. Operator overwrites with the real value out-of-band; terraform ignores subsequent value drift (ignore_changes=[value]). NEVER put a real secret here."
  type        = string
  default     = "PLACEHOLDER_SET_REAL_VALUE_VIA_AWS_SSM_PUT_PARAMETER"
}

locals {
  # (service, key) pairs → /tickvault/<env>/<service>/<key>. Names mirror the
  # SSM_*_SERVICE + *_SECRET constants in crates/common/src/constants.rs so the
  # app reads exactly these paths.
  prod_ssm_secrets = {
    dhan_client_id       = { service = "dhan", key = "client-id" }
    dhan_client_secret   = { service = "dhan", key = "client-secret" }
    dhan_totp_secret     = { service = "dhan", key = "totp-secret" }
    api_bearer_token     = { service = "api", key = "bearer-token" }
    questdb_pg_user      = { service = "questdb", key = "pg-user" }
    questdb_pg_password  = { service = "questdb", key = "pg-password" }
    telegram_bot_token   = { service = "telegram", key = "bot-token" }
    telegram_chat_id     = { service = "telegram", key = "chat-id" }
    network_static_ip    = { service = "network", key = "static-ip" }
    operator_github_token = { service = "operator", key = "github-token" }
  }
}

resource "aws_ssm_parameter" "prod_secrets" {
  for_each = local.prod_ssm_secrets

  name        = "/tickvault/${var.environment}/${each.value.service}/${each.value.key}"
  description = "tickvault prod secret shell (${each.value.service}/${each.value.key}) — value set by operator out-of-band, terraform owns the shell only."
  type        = "SecureString"
  value       = var.ssm_placeholder_value

  tags = {
    Name      = "tv-${var.environment}-ssm-${each.key}"
    ManagedBy = "terraform"
    SecretFor = each.value.service
    Note      = "value-set-out-of-band"
  }

  lifecycle {
    # Operator owns the VALUE (set out-of-band). Terraform owns the param's
    # existence + type only. Never revert a real secret to the placeholder.
    ignore_changes = [value]
  }
}

output "prod_ssm_parameter_names" {
  description = "Names of the prod SSM SecureString shells terraform created. Operator MUST populate each with the real value via `aws ssm put-parameter --overwrite` before the data-only prod cutover boots (mode=live runs the auth + static-IP gates)."
  value       = [for p in aws_ssm_parameter.prod_secrets : p.name]
}
