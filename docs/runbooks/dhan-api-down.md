# Runbook: Dhan API Goes Down

## Scenario
Dhan's servers are unreachable or returning 5xx errors.
This has happened historically during volatile market opens.

## Detection
- System detects WebSocket disconnect automatically
- REST API calls returning 5xx
- Alert fires within 30 seconds: "Dhan API unreachable"

## Immediate Actions

### Step 1: Check Dhan status
- **Status page:** https://status.dhanhq.co (bookmark NOW)
- **Dhan Twitter/X** for announcements
- Indian trader communities for reports

### Step 2: Assess your exposure
- Check existing positions on **Dhan mobile app**
- Positions are held at exchange level — they are SAFE
- System going down does NOT close your positions

### Step 3: Wait or act manually

**If Dhan is down for < 15 minutes:**
- Wait — system will auto-reconnect

**If Dhan is down for > 15 minutes:**
- Monitor positions manually on app
- Make manual decisions on Dhan app if needed

### Step 4: After Dhan recovers
1. System auto-reconnects and resumes
2. Run reconciliation before trusting any state
3. Check QuestDB for data gaps during outage:
   ```sql
   SELECT min(ts), max(ts), count() FROM ticks
   WHERE ts > dateadd('h', -2, now())
   ```

## System Requirements Verified By Tests
- Exponential backoff on reconnect (no Dhan server hammering)
- Alert distinguishes "Dhan down" from "my internet down"
- Never assumes position state during outage
- Test: `gap_enforcement::ws_disconnect_codes::*`
- Test: `safety_layer::alert_routing::*`
