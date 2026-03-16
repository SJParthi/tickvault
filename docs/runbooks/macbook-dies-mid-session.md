# Runbook: MacBook Dies Mid-Session

## Scenario
MacBook crashes or loses power during market hours with open F&O positions.

## Immediate Actions (on phone, in order)

### Step 1: Check positions (< 2 minutes)
- Open **Dhan app** on phone
- Check current positions and P&L
- Are any positions at dangerous levels?

### Step 2: Assess risk (< 3 minutes)
- How many open positions?
- How much capital is at risk?
- Is market moving against you significantly?

### Step 3: Decision point

**If positions are small and market is stable:**
- Let them run. Dhan holds them at exchange level.
- Restart system on backup machine or after restart.

**If positions are large or market is volatile:**
- Manually square off on Dhan app.
- Your fingers are the kill switch.

### Step 4: After restart
1. System will reconnect automatically (WebSocket auto-reconnect).
2. Run manual reconciliation: `cargo run --bin dhan-live-trader -- --reconcile-only`
3. Verify Valkey, QuestDB, Dhan API all agree.
4. Check for missed tick gaps in QuestDB:
   ```sql
   SELECT min(ts), max(ts) FROM ticks WHERE ts > dateadd('h', -4, now())
   ```

## System Requirements Verified By Tests
- System survives unexpected shutdown without data corruption (Valkey AOF, QuestDB WAL)
- WebSocket auto-reconnects on restart
- Reconciliation detects any state drift
- Test: `safety_layer::reconciliation_safety::*`

## Key Contacts
- Dhan support: via Dhan app chat
- Status page: https://status.dhanhq.co
