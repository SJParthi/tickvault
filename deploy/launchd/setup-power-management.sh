#!/bin/bash
# C3: Mac power management setup for trading
#
# Run ONCE after OS install. Configures:
#   - System never sleeps when on AC power
#   - Display sleeps after 10 minutes (save screen)
#   - Disk never sleeps
#   - Wake on LAN enabled
#   - Wake at 07:55 AM weekdays (safety net, 5 min before 08:00 launchd trigger)
#
# Usage: sudo bash deploy/launchd/setup-power-management.sh
#
# Verify: pmset -g

set -euo pipefail

echo "=== Dhan Live Trader — Mac Power Management Setup ==="
echo ""

# Require root
if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: Must run as root (sudo)"
    exit 1
fi

# AC power settings (charger connected)
echo "Setting AC power management..."
pmset -c sleep 0          # System never sleeps
pmset -c displaysleep 10  # Display sleeps after 10 min
pmset -c disksleep 0      # Disk never sleeps
pmset -c womp 1           # Wake on LAN enabled

# Wake schedule — 07:55 AM weekdays (safety net before 08:00 launchd trigger)
echo "Setting wake schedule (07:55 AM weekdays)..."
pmset repeat wakeorpoweron MTWRF 07:55:00

echo ""
echo "=== Configuration applied ==="
echo ""
echo "Current settings:"
pmset -g
echo ""
echo "Wake schedule:"
pmset -g sched
echo ""
echo "IMPORTANT:"
echo "  1. Enable auto-login: System Preferences > Users > Login Options"
echo "  2. Disable automatic macOS updates during market hours"
echo "  3. Disable Docker Desktop auto-updates"
echo "  4. Install the launchd plist:"
echo "     cp deploy/launchd/co.dhan.live-trader.plist ~/Library/LaunchAgents/"
echo "     launchctl load ~/Library/LaunchAgents/co.dhan.live-trader.plist"
