#!/usr/bin/env bash
# =============================================================================
# tickvault — CloudWatch External Health Alarms
# =============================================================================
# Covers the 2 scenarios that local monitoring CANNOT self-detect:
#   1. EC2 instance death (StatusCheckFailed → SNS → your phone)
#   2. Docker daemon crash (custom metric via CloudWatch agent → SNS)
#
# WHY: A dead machine can't report its own death. Prometheus, Grafana, and
# the Rust app all run ON the same host. If the host dies, they all die.
# CloudWatch runs in AWS — it watches FROM OUTSIDE.
#
# WHEN TO RUN: Once, after deploying to AWS EC2 (c7i.2xlarge Mumbai).
#              Idempotent — safe to re-run.
#
# PREREQUISITES:
#   - AWS CLI configured with ap-south-1 region
#   - EC2 instance running with instance ID
#   - SNS phone number in SSM at /tickvault/<env>/sns/phone-number
#
# Usage:  ./scripts/setup-cloudwatch-alarms.sh [--env dev|prod] [--instance-id i-xxx]
# =============================================================================

set -euo pipefail

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

ok()   { echo -e "  ${GREEN}✓${NC} $1"; }
fail() { echo -e "  ${RED}✗${NC} $1"; }
info() { echo -e "  ${CYAN}ℹ${NC} $1"; }
warn() { echo -e "  ${YELLOW}⚠${NC} $1"; }

REGION="ap-south-1"
SSM_ENV="dev"
INSTANCE_ID=""

# Parse args
while [[ $# -gt 0 ]]; do
    case "$1" in
        --env)        SSM_ENV="$2"; shift 2 ;;
        --instance-id) INSTANCE_ID="$2"; shift 2 ;;
        *) echo "Unknown arg: $1"; exit 1 ;;
    esac
done

echo ""
echo -e "${CYAN}╔════════════════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║  tickvault — CloudWatch External Health Alarms  ║${NC}"
echo -e "${CYAN}╚════════════════════════════════════════════════════════╝${NC}"
echo ""

# ---- Step 1: Validate prerequisites ----
info "Region: ${REGION}"
info "Environment: ${SSM_ENV}"

if ! command -v aws &>/dev/null; then
    fail "AWS CLI not found. Install: https://docs.aws.amazon.com/cli/latest/userguide/install-cliv2.html"
    exit 1
fi

# Auto-detect instance ID if not provided (only works ON the EC2 instance)
if [ -z "$INSTANCE_ID" ]; then
    info "No --instance-id provided. Attempting auto-detect via IMDS..."
    TOKEN=$(curl -sf --max-time 2 -X PUT "http://169.254.169.254/latest/api/token" \
        -H "X-aws-ec2-metadata-token-ttl-seconds: 30" 2>/dev/null || echo "")
    if [ -n "$TOKEN" ]; then
        INSTANCE_ID=$(curl -sf --max-time 2 "http://169.254.169.254/latest/meta-data/instance-id" \
            -H "X-aws-ec2-metadata-token: $TOKEN" 2>/dev/null || echo "")
    fi
    if [ -z "$INSTANCE_ID" ]; then
        fail "Could not auto-detect instance ID. Run with: --instance-id i-0abc123..."
        exit 1
    fi
fi
ok "Instance ID: ${INSTANCE_ID}"

# ---- Step 2: Create or find SNS topic ----
TOPIC_NAME="tv-${SSM_ENV}-health-alarms"
info "Creating SNS topic: ${TOPIC_NAME}"

TOPIC_ARN=$(aws sns create-topic \
    --name "$TOPIC_NAME" \
    --region "$REGION" \
    --query 'TopicArn' \
    --output text 2>/dev/null)

if [ -z "$TOPIC_ARN" ]; then
    fail "Could not create SNS topic"
    exit 1
fi
ok "SNS topic: ${TOPIC_ARN}"

# ---- Step 3: Subscribe phone number from SSM ----
PHONE_NUMBER=$(aws ssm get-parameter \
    --name "/tickvault/${SSM_ENV}/sns/phone-number" \
    --with-decryption \
    --region "$REGION" \
    --query 'Parameter.Value' \
    --output text 2>/dev/null || echo "")

if [ -n "$PHONE_NUMBER" ]; then
    # Subscribe (idempotent — AWS deduplicates)
    aws sns subscribe \
        --topic-arn "$TOPIC_ARN" \
        --protocol sms \
        --notification-endpoint "$PHONE_NUMBER" \
        --region "$REGION" \
        --output text >/dev/null 2>&1
    ok "SMS subscription: ${PHONE_NUMBER}"
else
    warn "No phone number in SSM at /tickvault/${SSM_ENV}/sns/phone-number — alarms will fire but SMS won't deliver"
fi

# Also subscribe Telegram via a Lambda (if exists) — future enhancement
# For now, SNS → SMS is the external alert path.

# ---- Step 4: Alarm 1 — EC2 Instance Death ----
ALARM_NAME_INSTANCE="tv-${SSM_ENV}-ec2-instance-health"
info "Creating alarm: ${ALARM_NAME_INSTANCE}"

aws cloudwatch put-metric-alarm \
    --alarm-name "$ALARM_NAME_INSTANCE" \
    --alarm-description "CRITICAL: tv-${SSM_ENV} EC2 instance has failed status checks. The entire trading system is DOWN. Immediate action required." \
    --namespace "AWS/EC2" \
    --metric-name "StatusCheckFailed" \
    --dimensions "Name=InstanceId,Value=${INSTANCE_ID}" \
    --statistic "Maximum" \
    --period 60 \
    --evaluation-periods 2 \
    --threshold 1 \
    --comparison-operator "GreaterThanOrEqualToThreshold" \
    --alarm-actions "$TOPIC_ARN" \
    --ok-actions "$TOPIC_ARN" \
    --treat-missing-data "breaching" \
    --region "$REGION" 2>/dev/null

ok "Alarm created: EC2 StatusCheckFailed → SMS within 2 minutes"

# ---- Step 5: Alarm 2 — Docker Daemon Health ----
# Uses CloudWatch agent custom metric. The agent publishes a heartbeat metric
# every 60s. If the metric STOPS (missing data = breaching), alarm fires.
ALARM_NAME_DOCKER="tv-${SSM_ENV}-docker-daemon-health"
info "Creating alarm: ${ALARM_NAME_DOCKER}"

aws cloudwatch put-metric-alarm \
    --alarm-name "$ALARM_NAME_DOCKER" \
    --alarm-description "CRITICAL: Docker daemon on tv-${SSM_ENV} is not responding. Containers may be down. Check systemd: systemctl status docker." \
    --namespace "DLT/Infrastructure" \
    --metric-name "DockerDaemonHealthy" \
    --dimensions "Name=InstanceId,Value=${INSTANCE_ID}" \
    --statistic "Minimum" \
    --period 60 \
    --evaluation-periods 3 \
    --threshold 1 \
    --comparison-operator "LessThanThreshold" \
    --alarm-actions "$TOPIC_ARN" \
    --ok-actions "$TOPIC_ARN" \
    --treat-missing-data "breaching" \
    --region "$REGION" 2>/dev/null

ok "Alarm created: Docker daemon heartbeat missing → SMS within 3 minutes"

# ---- Step 6: Install CloudWatch agent cron for Docker heartbeat ----
# This publishes a custom metric every 60s proving Docker is alive.
CRON_SCRIPT="/opt/tickvault/docker-heartbeat.sh"
info "Installing Docker heartbeat publisher at ${CRON_SCRIPT}"

sudo mkdir -p /opt/dlt 2>/dev/null || true

sudo tee "$CRON_SCRIPT" > /dev/null << 'HEARTBEAT_EOF'
#!/usr/bin/env bash
# Published every 60s by cron. If this stops, CloudWatch alarm fires.
if docker info >/dev/null 2>&1; then
    VALUE=1
else
    VALUE=0
fi
INSTANCE_ID=$(curl -sf --max-time 2 -X PUT "http://169.254.169.254/latest/api/token" \
    -H "X-aws-ec2-metadata-token-ttl-seconds: 30" 2>/dev/null | \
    xargs -I{} curl -sf --max-time 2 "http://169.254.169.254/latest/meta-data/instance-id" \
    -H "X-aws-ec2-metadata-token: {}" 2>/dev/null || echo "unknown")
aws cloudwatch put-metric-data \
    --namespace "DLT/Infrastructure" \
    --metric-name "DockerDaemonHealthy" \
    --dimensions "InstanceId=${INSTANCE_ID}" \
    --value "$VALUE" \
    --unit "None" \
    --region "ap-south-1" 2>/dev/null
HEARTBEAT_EOF

sudo chmod +x "$CRON_SCRIPT" 2>/dev/null || true

# Add cron job (idempotent)
CRON_LINE="* * * * * $CRON_SCRIPT"
if ! sudo crontab -l 2>/dev/null | grep -qF "$CRON_SCRIPT"; then
    (sudo crontab -l 2>/dev/null; echo "$CRON_LINE") | sudo crontab -
    ok "Cron installed: Docker heartbeat every 60s"
else
    ok "Cron already installed"
fi

# ---- Summary ----
echo ""
echo -e "${CYAN}══════════════════════════════════════════════════════${NC}"
echo -e "  ${GREEN}2 CloudWatch alarms created:${NC}"
echo ""
echo -e "  ${CYAN}1. EC2 Instance Death${NC}"
echo -e "     Alarm:   ${ALARM_NAME_INSTANCE}"
echo -e "     Trigger: StatusCheckFailed for 2 consecutive minutes"
echo -e "     Action:  SMS to ${PHONE_NUMBER:-'(no phone in SSM)'}"
echo ""
echo -e "  ${CYAN}2. Docker Daemon Crash${NC}"
echo -e "     Alarm:   ${ALARM_NAME_DOCKER}"
echo -e "     Trigger: Heartbeat metric missing for 3 minutes"
echo -e "     Action:  SMS to ${PHONE_NUMBER:-'(no phone in SSM)'}"
echo ""
echo -e "  ${CYAN}SNS Topic:${NC} ${TOPIC_ARN}"
echo -e "${CYAN}══════════════════════════════════════════════════════${NC}"
echo ""
echo -e "${GREEN}External health monitoring active. These alarms work even when the entire host is dead.${NC}"
echo ""
exit 0
