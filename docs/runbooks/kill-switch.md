# Runbook — Kill Switch (disable all trading for the day)

> **What it is:** Dhan's account-level Kill Switch. When ON, the exchange
> **blocks every new order for the rest of the trading day** for your client
> id. It is the emergency "stop trading NOW" lever.
> **Ground truth:** `docs/dhan-ref/15-traders-control.md`.
> **Live code:** `crates/trading/src/oms/api_client.rs` —
> `activate_kill_switch` / `deactivate_kill_switch` / `get_kill_switch_status`.
> **Endpoint base:** `https://api.dhan.co/v2/killswitch`
> (`DHAN_KILL_SWITCH_PATH = "/killswitch"`).

---

## When to pull it

| Trigger | Why |
|---|---|
| `🆘 Option-chain STRATEGY HALTED` Telegram (`OptionChainStaleHalt`) | Strategy can't see fresh option data; if you can't restore it fast, kill trading rather than fly blind. |
| Runaway / unexpected orders, reconciliation mismatch, circuit-breaker storm | Stop the bleeding first, diagnose second. |
| Any "I'm not sure what the system is doing" moment during market hours | Safer closed than open. |

**Rule of thumb:** kill first, investigate after. Re-enabling is one call.

---

## The three operations

| Action | Call | HTTP |
|---|---|---|
| **Turn ON** (block all new orders) | `activate_kill_switch` | `POST /v2/killswitch?killSwitchStatus=ACTIVATE` |
| **Turn OFF** (re-enable trading) | `deactivate_kill_switch` | `POST /v2/killswitch?killSwitchStatus=DEACTIVATE` |
| **Check state** | `get_kill_switch_status` | `GET /v2/killswitch` → `{ "killSwitchStatus": "ACTIVATE" | "DEACTIVATE" }` |

All three require the `access-token` header (the live JWT).

---

## ⚠️ Prerequisite to ACTIVATE

Dhan rejects ACTIVATE unless **all positions are closed and there are no
pending orders**. So the emergency sequence is:

1. **Exit everything** — `exit_all_positions()` (`DELETE /v2/positions`) cancels
   pending orders and squares off open positions.
2. **Confirm flat** — check the position book is empty.
3. **Activate** — `activate_kill_switch`.

If you ACTIVATE without flattening first, the call fails and trading is **not**
stopped — always do step 1 first.

---

## Scope + lifetime

- **Day-scoped.** The Kill Switch state is for the **current trading day**; it
  does not silently persist a permanent ban, but **do not assume** it auto-clears
  — verify with `get_kill_switch_status` at next session start.
- **Account-wide.** It blocks orders from this app AND from the Dhan web/app —
  it is enforced exchange-side per `dhanClientId`, not per connection.

---

## Operator steps (manual, during an incident)

1. **Flatten:** trigger exit-all (cancels pending + squares off). See the
   positions/portfolio API.
2. **Activate** the Kill Switch.
3. **Verify** `get_kill_switch_status` returns `ACTIVATE`.
4. **Diagnose** the root cause calmly — the account is now safe.
5. **Re-enable** only when you understand what happened: `deactivate_kill_switch`,
   then verify status is `DEACTIVATE`.

---

## Related: P&L-based auto-exit (`/pnlExit`)

A complementary, automatic guard (separate from the manual Kill Switch):

- `POST /v2/pnlExit` — auto-exit + optionally auto-activate the Kill Switch when
  a profit or loss threshold is hit (`enableKillSwitch: true`).
- ⚠️ If you set `lossValue` **below** the current loss (or `profitValue` below
  current profit), the exit fires **immediately**.
- Active for the current day only; resets at end of session.
- Note: its `productType` array uses `"DELIVERY"` (not `"CNC"`) for cash positions.

Use P&L-exit to pre-arm a safety net; use the manual Kill Switch for "stop now".

---

## What NOT to do

- ❌ Don't ACTIVATE before flattening — the call fails, trading stays open.
- ❌ Don't assume DEACTIVATE happened — always confirm with the status check.
- ❌ Don't leave it ON across days without a note — next session's pre-open check
  should verify the switch is OFF before the strategy expects to trade.

---

## Cross-references

- `docs/dhan-ref/15-traders-control.md` — Dhan API ground truth.
- `crates/core/src/notification/events.rs` — the `OptionChainStaleHalt` event
  that points the operator here.
