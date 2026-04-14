# Runbook: Internet Goes Down Mid-Session

## Scenario
Home internet fails during market hours.

## Immediate Actions

### Step 1: Switch to mobile hotspot IMMEDIATELY
- Enable phone hotspot (keep it pre-configured)
- Laptop WiFi connects to hotspot automatically if configured

### Step 2: Verify system reconnected
- Check Grafana dashboard (accessible via phone hotspot)
- Verify WebSocket reconnected (green indicator)
- Verify tick ingestion resumed (check last tick timestamp)

### Step 3: Check for missed data
- Note the gap time (internet down from X to Y)
- QuestDB will have a gap in tick data for those minutes
- This is acceptable — log it, move on

### Step 4: If reconnect took > 5 minutes
- Verify position state via reconciliation
- Run: `cargo run --bin tickvault -- --reconcile-only`
- Do not trust cached state until reconciliation passes

## System Requirements Verified By Tests
- WebSocket auto-reconnects on internet restore
- NEVER-009: Reconnect must NOT create duplicate subscriptions
- Reconnect re-fetches missed instrument states from REST API
- Test: `gap_enforcement::ws_disconnect_codes::*`
- Test: `safety_layer::never_requirements::*`

## Pre-Configuration Checklist
- [ ] Phone hotspot SSID saved on laptop
- [ ] Phone hotspot auto-connect enabled
- [ ] Grafana accessible on hotspot network
