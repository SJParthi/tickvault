# Plan: WebSocket disconnect CAUSE classifier — human-readable AWS / network / Dhan attribution

**Status:** DRAFT
**Date:** 2026-06-12
**Approved by:** pending (operator requested this be QUEUED 2026-06-12)
**Branch:** claude/nice-cerf-hemuvn
**Changed crates (when implemented):** `common` (classifier enum + pure fn), `core` (notification events + connection wiring)

## Why (operator request, 2026-06-12)
Today the `WebSocketDisconnected` / `WebSocketReconnected` Telegram shows the RAW
transport error (e.g. "Connection reset without closing handshake", "Handshake
not finished") + down-secs + attempts — but it does NOT tell the operator the
likely **source**: our AWS box, the network path, or Dhan itself. A source
classifier was never committed (verified via git `-S` across all branches). This
adds it.

## Design
A pure, O(1) classifier `classify_disconnect_cause(&err, dhan_code: Option<u16>) -> DisconnectCause`
in `crates/common` mapping the transport error kind + (optional) Dhan disconnect
code to a best-guess category, plus a one-line plain-English explanation + a
"how to confirm" hint. Wired into the `reason` shown by
`WebSocketDisconnected/Reconnected` so every alert reads e.g.:

> WebSocket 1/1 disconnected
> Connection reset without closing handshake
> Likely source: **Dhan or network** (Dhan's server / the path to it dropped the line)
> Confirm: if other Dhan calls also failed → network; if only the feed → Dhan-side.

## Scenario map (the categories + detection)
| Symptom (transport error / Dhan code) | Best-guess source | Detection signal | Operator hint |
|---|---|---|---|
| `ResetWithoutClosingHandshake` mid-stream | Dhan **or** network | tungstenite `Protocol(ResetWithoutClosingHandshake)` | correlate with other Dhan calls |
| `Handshake not finished` / TLS fail on connect | network / TLS / our-AWS egress | error in connect phase | check AWS egress + DNS |
| DNS resolution failure | our-AWS / network DNS | `io::ErrorKind` + "dns"/"resolve" | check resolver on box |
| Connection refused / connect timeout | network / Dhan-down | `io::ErrorKind::{ConnectionRefused,TimedOut}` | check Dhan status page |
| Dhan disconnect code **805** | **Dhan-side: too many connections** (dual login) | `DisconnectCode::TooManyConnections` | close other Dhan logins |
| Dhan disconnect code **807** | **Dhan-side: token expired** | `DisconnectCode::AccessTokenExpired` | token auto-refresh path |
| Dhan disconnect **806/808/809/810** | Dhan-side: auth/subscription | `DisconnectCode` | check data plan / auth |
| repeated resets in a burst (RST flood) | network | rate of resets | network blip / MTU |
| our-AWS ENA / instance network event | **AWS-side** | correlate w/ CloudWatch instance net metrics | check EC2 status checks |
| anything unmatched | **Unknown** (show raw) | fallback | raw error + "investigate" |

## Edge Cases
- Transport reset with NO Dhan code → cannot be 100% attributed → category `DhanOrNetwork` with explicit "can't be certain" wording (no false attribution).
- Off-hours disconnects (post-15:30) → keep the existing `WebSocketDisconnectedOffHours` Severity::Low path; classify but don't page.
- Order-update WS uses the same classifier (it has its own `classify_auth_response` for auth, distinct).

## Failure Modes
- Misclassification: bounded by the honest envelope — the message always shows the RAW error too, so a wrong bucket never hides the truth.
- No new panic path: classifier is a pure `match` with a total fallback arm (`Unknown`).

## Test Plan
- Unit: `classify_disconnect_cause` table-driven over every row above (one test per category + the Unknown fallback).
- Formatting test: the Telegram body contains the raw error AND the source line.
- Edge: 805/807 map to the exact Dhan-side categories (cross-check `DisconnectCode`).

## Rollback
Single-commit revert restores the raw-reason-only message; classifier is additive.

## Observability
The whole point: richer, human-readable disconnect Telegram. No new metric (the
existing `tv_websocket_*` counters stay); the classifier output is a label on the
existing alert.

## Per-Wave Guarantee Matrix (cross-reference)
See `.claude/rules/project/per-wave-guarantee-matrix.md` — all 15 rows of the 100%
Guarantee Matrix and all 7 rows of the Resilience Demand Matrix apply to every
item in this plan (classifier is O(1) pure-fn on a cold path; uniqueness/dedup
N/A; the advanced rows are 100% monitoring / logging / alerting + 100% scenario
coverage via the table-driven tests).

## Honest 100% claim
100% inside the tested envelope: every transport-error kind + Dhan code in the
scenario map is table-tested and produces a deterministic category + raw error.
**Caveat (no hallucination):** a bare TCP reset cannot be perfectly attributed to
AWS-vs-network-vs-Dhan from a single event — those map to a best-guess bucket
with explicit uncertainty wording + a confirm-hint, never a false certainty.

## Plan Items
- [ ] `DisconnectCause` enum + `classify_disconnect_cause` pure fn + tests (common)
- [ ] Wire classifier into `WebSocketDisconnected/Reconnected` reason (core events + connection.rs)
- [ ] Table-driven unit tests for every scenario row + Unknown fallback
- [ ] 3-agent adversarial review on the diff
