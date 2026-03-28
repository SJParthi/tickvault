# Dhan Trader's Control Enforcement

> **Ground truth:** `docs/dhan-ref/15-traders-control.md`
> **Scope:** Any file touching kill switch, P&L-based auto-exit, or emergency trading halt.
> **Cross-reference:** `docs/dhan-ref/12-portfolio-positions.md` (exit-all as alternative)

## Mechanical Rules

1. **Read the ground truth first.** Before adding, modifying, or reviewing any kill switch or P&L exit handler: `Read docs/dhan-ref/15-traders-control.md`.

2. **Five endpoints — exact URLs.**
   - Kill switch activate/deactivate: `POST /v2/killswitch?killSwitchStatus=ACTIVATE|DEACTIVATE`
   - Kill switch status: `GET /v2/killswitch`
   - P&L exit configure: `POST /v2/pnlExit`
   - P&L exit stop: `DELETE /v2/pnlExit`
   - P&L exit status: `GET /v2/pnlExit`

3. **Kill switch prerequisite:** All positions must be closed and no pending orders to activate. If positions exist, close them first (via exit-all or individual orders).

4. **Kill switch disables ALL trading for the day.** Use as emergency stop only.

5. **P&L exit values are STRINGS** (`"1500.00"`, `"500.00"`), not floats.

6. **P&L exit `productType` is an array** of strings: `["INTRADAY", "DELIVERY"]`.

7. **WARNING: If `profitValue` < current profit OR `lossValue` < current loss, exit triggers IMMEDIATELY.** Always check current P&L before configuring.

8. **P&L exit is session-scoped** — resets at end of trading session. Must reconfigure daily if needed.

9. **`enableKillSwitch: true`** in P&L exit = after auto-exit, also activates kill switch to prevent further trading.

10. **Kill switch status response:** `{ "dhanClientId": "...", "killSwitchStatus": "ACTIVATE" }` or `"DEACTIVATE"`.

11. **P&L exit POST response:** `{ "pnlExitStatus": "ACTIVE", "message": "..." }`.

12. **P&L exit GET response uses DIFFERENT field names from POST request:**
    - POST request: `profitValue`, `lossValue`
    - GET response: `profit`, `loss` (shorter names)
    - GET response: `{ "pnlExitStatus": "ACTIVE", "profit": "1500.00", "loss": "500.00", "productType": [...], "enable_kill_switch": true }`
    - Note: `enable_kill_switch` uses snake_case in GET response (inconsistent with `enableKillSwitch` camelCase in POST).

13. **P&L exit DELETE response:** `{ "pnlExitStatus": "DISABLED", "message": "..." }`.

14. **For dhan-live-trader:** Wire kill switch activation into error handlers. On critical errors during live trading → call kill switch immediately.

## What This Prevents

- Activating kill switch with open positions → API error → no emergency stop when needed
- P&L threshold below current P&L → immediate unintended exit of all positions
- Forgetting P&L exit is session-scoped → no protection on subsequent days
- Not wiring kill switch to error handlers → runaway losses on system failure

## Trigger

This rule activates when editing files matching:
- `crates/trading/src/risk/kill_switch.rs`
- `crates/trading/src/risk/pnl_exit.rs`
- `crates/trading/src/oms/emergency*.rs`
- `crates/core/src/shutdown/*.rs`
- Any file containing `KillSwitch`, `killswitch`, `killSwitchStatus`, `PnlExit`, `pnlExit`, `profitValue`, `lossValue`, `enableKillSwitch`, `emergency_halt`
