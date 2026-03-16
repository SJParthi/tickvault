#!/bin/bash
# D3: EC2 setup script for dhan-live-trader
#
# Run ONCE on a fresh Amazon Linux 2023 / Ubuntu EC2 instance.
# Installs Docker, configures log rotation, sets up systemd units,
# installs CloudWatch agent, and configures time synchronization.
#
# Prerequisites:
#   - EC2 instance with IAM role (SSM, CloudWatch, logs permissions)
#   - Elastic IP attached (for Dhan static IP whitelist)
#   - Security group: outbound 443 (HTTPS), inbound 22 (SSH from admin IP)
#
# Usage: sudo bash deploy/aws/ec2-setup.sh

set -euo pipefail

echo "=== Dhan Live Trader — EC2 Setup ==="
echo ""

# ---- Mac cleanup guard (automatic — no sentinel needed) ----
# If this script exists in a git repo that still has deploy/launchd/,
# refuse to proceed. Forces cleanup before ANY AWS deployment.
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
if [ -d "$REPO_ROOT/deploy/launchd" ]; then
    echo "╔══════════════════════════════════════════════════════════════╗"
    echo "║  BLOCKED: deploy/launchd/ still exists in repo             ║"
    echo "║  Delete ALL Mac local infra before deploying to AWS.       ║"
    echo "║  Run: bash scripts/aws-migration-guard.sh                  ║"
    echo "║  See: .claude/rules/project/aws-migration.md               ║"
    echo "╚══════════════════════════════════════════════════════════════╝"
    exit 1
fi

# Require root
if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: Must run as root (sudo)"
    exit 1
fi

# -------------------------------------------------------------------------
# Step 1: Set timezone
# -------------------------------------------------------------------------
echo "[1/7] Setting timezone to Asia/Kolkata..."
timedatectl set-timezone Asia/Kolkata

# -------------------------------------------------------------------------
# Step 2: Install Docker
# -------------------------------------------------------------------------
echo "[2/7] Installing Docker..."
if command -v docker &> /dev/null; then
    echo "  Docker already installed: $(docker --version)"
else
    # Amazon Linux 2023
    if [ -f /etc/amazon-linux-release ]; then
        dnf install -y docker docker-compose-plugin
    # Ubuntu
    elif [ -f /etc/lsb-release ]; then
        apt-get update
        apt-get install -y docker.io docker-compose-v2
    else
        echo "ERROR: Unsupported OS. Install Docker manually."
        exit 1
    fi
fi

systemctl enable docker
systemctl start docker

# Add ec2-user to docker group (avoids sudo for docker commands)
usermod -aG docker ec2-user 2>/dev/null || usermod -aG docker ubuntu 2>/dev/null || true

# -------------------------------------------------------------------------
# Step 3: Configure Docker log rotation
# -------------------------------------------------------------------------
echo "[3/7] Configuring Docker log rotation..."
mkdir -p /etc/docker
cat > /etc/docker/daemon.json << 'DOCKER_CONF'
{
    "log-driver": "json-file",
    "log-opts": {
        "max-size": "50m",
        "max-file": "5"
    }
}
DOCKER_CONF

systemctl restart docker

# -------------------------------------------------------------------------
# Step 4: Configure time synchronization (Amazon Time Sync Service)
# -------------------------------------------------------------------------
echo "[4/7] Configuring chrony for Amazon Time Sync Service..."
if command -v chronyc &> /dev/null; then
    # Amazon Linux 2023 ships with chrony pre-configured for 169.254.169.123
    chronyc tracking
    echo "  chrony is configured"
else
    echo "  WARNING: chrony not found — install manually for accurate timestamps"
fi

# -------------------------------------------------------------------------
# Step 5: Create application directory
# -------------------------------------------------------------------------
echo "[5/7] Creating application directory..."
mkdir -p /opt/dhan-live-trader/bin
mkdir -p /opt/dhan-live-trader/config
mkdir -p /opt/dhan-live-trader/deploy/docker
mkdir -p /opt/dhan-live-trader/logs

# -------------------------------------------------------------------------
# Step 6: Install systemd units
# -------------------------------------------------------------------------
echo "[6/7] Installing systemd units..."
SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"

cp "${SCRIPT_DIR}/systemd/dhan-live-trader.service" /etc/systemd/system/
cp "${SCRIPT_DIR}/systemd/dhan-live-trader.timer" /etc/systemd/system/
systemctl daemon-reload
systemctl enable dhan-live-trader.timer

echo "  Timer enabled. Next trigger:"
systemctl list-timers dhan-live-trader.timer --no-pager

# -------------------------------------------------------------------------
# Step 7: Install CloudWatch agent (optional)
# -------------------------------------------------------------------------
echo "[7/7] Checking CloudWatch agent..."
if command -v amazon-cloudwatch-agent-ctl &> /dev/null; then
    echo "  CloudWatch agent already installed"
else
    echo "  CloudWatch agent not installed. Install manually for memory/disk metrics:"
    echo "    sudo dnf install -y amazon-cloudwatch-agent  # Amazon Linux 2023"
    echo "    # Then configure with: amazon-cloudwatch-agent-ctl -a fetch-config -s"
fi

# -------------------------------------------------------------------------
# Step 8: Set SSM parameter (fallback safety net)
# -------------------------------------------------------------------------
echo "[8/9] Setting SSM deployment-mode parameter..."
if command -v aws &> /dev/null; then
    aws ssm put-parameter \
        --name "/dhan-live-trader/deployment/primary-host" \
        --value "aws" \
        --type "String" \
        --overwrite 2>/dev/null && \
        echo "  SSM parameter set: primary-host=aws" || \
        echo "  WARNING: Could not set SSM parameter"
fi

# -------------------------------------------------------------------------
# Step 9: Decommission Mac automatically via SSH
# -------------------------------------------------------------------------
# Reads Mac SSH details from SSM. If not set, prompts for them.
# This is the step that ACTUALLY kills the Mac — unloads launchd,
# removes plist, stops containers, creates decommission flag.
echo "[9/9] Decommissioning Mac trading host via SSH..."

MAC_HOST=""
MAC_USER=""

# Try SSM first
if command -v aws &> /dev/null; then
    MAC_HOST=$(aws ssm get-parameter \
        --name "/dhan-live-trader/mac/ssh-host" \
        --query "Parameter.Value" --output text 2>/dev/null || echo "")
    MAC_USER=$(aws ssm get-parameter \
        --name "/dhan-live-trader/mac/ssh-user" \
        --query "Parameter.Value" --output text 2>/dev/null || echo "")
fi

# Prompt if not in SSM
if [ -z "$MAC_HOST" ]; then
    echo "  Mac SSH host not found in SSM."
    echo "  Enter Mac IP/hostname (or 'skip' to decommission manually later):"
    read -r MAC_HOST
fi
if [ "$MAC_HOST" = "skip" ] || [ -z "$MAC_HOST" ]; then
    echo "  SKIPPED: Mac decommission."
    echo ""
    echo "  ╔════════════════════════════════════════════════════════════╗"
    echo "  ║  WARNING: Mac is still active! Run this ON YOUR MAC:     ║"
    echo "  ║  bash deploy/launchd/decommission-mac.sh                 ║"
    echo "  ║                                                          ║"
    echo "  ║  Until you do, Mac will ALSO trade at 8:15 AM.           ║"
    echo "  ║  SSM safety net will block it, but launchd still fires.  ║"
    echo "  ╚════════════════════════════════════════════════════════════╝"
else
    if [ -z "$MAC_USER" ]; then
        MAC_USER="$(whoami)"
    fi

    echo "  Connecting to ${MAC_USER}@${MAC_HOST}..."

    # The decommission script path on the Mac
    MAC_DECOMMISSION="/Users/Shared/dhan-live-trader/deploy/launchd/decommission-mac.sh"

    if ssh -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new \
         "${MAC_USER}@${MAC_HOST}" "bash ${MAC_DECOMMISSION}" 2>&1; then
        echo ""
        echo "  Mac DECOMMISSIONED successfully via SSH."
        echo "  - launchd unloaded (no more 8:15 AM trigger)"
        echo "  - plist removed"
        echo "  - containers stopped"
        echo "  - decommission flag created"
    else
        echo ""
        echo "  WARNING: SSH to Mac failed. Decommission manually:"
        echo "    ssh ${MAC_USER}@${MAC_HOST} 'bash ${MAC_DECOMMISSION}'"
        echo ""
        echo "  SSM safety net is active — Mac launcher will check SSM"
        echo "  and auto-block at next 8:15 AM trigger. But launchd remains."
    fi
fi

echo ""
echo "=== Setup Complete ==="
echo ""
echo "Next steps:"
echo "  1. Deploy binary: scp target/release/dhan-live-trader ec2-user@<IP>:/opt/dhan-live-trader/bin/"
echo "  2. Deploy config: scp config/base.toml ec2-user@<IP>:/opt/dhan-live-trader/config/"
echo "  3. Deploy docker-compose: scp deploy/docker/docker-compose.yml ec2-user@<IP>:/opt/dhan-live-trader/deploy/docker/"
echo "  4. Verify timer: systemctl list-timers dhan-live-trader.timer"
echo "  5. Test manually: sudo systemctl start dhan-live-trader"
echo "  6. Check logs: journalctl -u dhan-live-trader -f"
