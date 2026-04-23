# 200-Level Depth WebSocket — Exhaustive Rust Client Variant Test Results

**To:** apihelp@dhan.co, support@dhan.co
**Subject:** Ticket #5519522 follow-up — Rust client fails in 8 configurations; Python SDK works on same account

**Status:** `DRAFT` — fill in `<PLACEHOLDERS>` after running
`cargo run --release --example depth_200_variants` at market open,
then `bash scripts/dhan-200-depth-repro/analyze_results.sh`.
Send only if analyze_results.sh exits 1 ("NO CLEAR WINNER").

---

Dear Dhan Support Team,

Ref: Ticket #5519522 (200-level depth `Protocol(ResetWithoutClosingHandshake)`)

**Account details (reconfirming so there is no lookup ambiguity):**

- Client ID: `1106656882`
- Name: `Parthiban S`
- UCC: `NWXF17021Q`
- API plan: Market Data API + Trading API (active)
- Static IP: configured and whitelisted

## Executive summary

Your official Python SDK `dhanhq==2.2.0rc1` **streams 200-level depth
successfully for 30+ minutes on our account** for SecurityId 72271
(NSE_FNO) at ATM option contracts.

Our Rust client connecting to the **same 200-level depth server with
the same account, same fresh JWT, same SecurityId, on the same
machine** gets TCP-reset (`Protocol(ResetWithoutClosingHandshake)`)
within seconds of subscribing — across **8 different URL /
User-Agent / ALPN configurations**.

This narrows the root cause to something in the Rust-side TLS or
WebSocket handshake that your server rejects. We need your help to
identify what exactly your server requires, since our SDK-level
attempts to match the Python SDK are not sufficient.

## Python SDK verification (proof account + token are fine)

Run on 2026-04-23 at `<IST_TIMESTAMP>`:

```python
from dhanhq import DhanContext, FullDepth
dhan_context = DhanContext("1106656882", "<JWT>")
instruments = [(2, "72271")]  # NSE_FNO, SecurityId 72271
response = FullDepth(dhan_context, instruments, 200)
response.run_forever()
# → Connected to ws://full-depth-api.dhan.co/?token=...&clientId=...&authType=2
# → Streamed 200-level depth for 30+ minutes, zero disconnects
```

SDK log excerpt attached as `python_sdk_success_2026-04-23.log`.

## Rust client test matrix

On 2026-04-24 `<IST_TIMESTAMP>` we ran 8 variants against the same
SecurityId 72271, same token, sequentially, each for 180 seconds.

| # | URL path | User-Agent | ALPN | Frames | Disconnects | Last disconnect reason |
|---|----------|------------|------|--------|-------------|------------------------|
| A | `/` | tungstenite default | `http/1.1` | `<FILL>` | `<FILL>` | `<FILL>` |
| B | `/twohundreddepth` | tungstenite default | `http/1.1` | `<FILL>` | `<FILL>` | `<FILL>` |
| C | `/` | `Python/3.12 websockets/16.0` | `http/1.1` | `<FILL>` | `<FILL>` | `<FILL>` |
| D | `/twohundreddepth` | `Python/3.12 websockets/16.0` | `http/1.1` | `<FILL>` | `<FILL>` | `<FILL>` |
| E | `/` | tungstenite default | none | `<FILL>` | `<FILL>` | `<FILL>` |
| F | `/` | `Chrome/126 + Origin + Pragma` | `http/1.1` | `<FILL>` | `<FILL>` | `<FILL>` |
| G | `/twohundreddepth` | tungstenite default | none | `<FILL>` | `<FILL>` | `<FILL>` |
| H | `/` | `Python/3.12 websockets/16.0` | none | `<FILL>` | `<FILL>` | `<FILL>` |

Full per-variant stderr logs attached as `rust_variants_2026-04-24.zip`.

## Environment

Identical across Python and Rust runs:

- Machine: `<hostname>`, MacBook Pro M2
- OS: macOS `<version>`
- Public IP (static, whitelisted): `<IP>`
- TLS trust store: macOS native root CA store (both Python's openssl
  and Rust's rustls read from the same system trust anchors)
- Network route to `full-depth-api.dhan.co`: direct, no proxy, no VPN

## Rust client library stack

- `tokio = 1.49.0`
- `tokio-tungstenite = 0.29.0` (features: `rustls-tls-native-roots`)
- `rustls = 0.23.x` with `aws-lc-rs` CryptoProvider
- `rustls-native-certs = 0.7.x`

## What works

- Live Market Feed WebSocket (`wss://api-feed.dhan.co?version=2&...`)
  — our Rust client streams **all 25,000 instruments**, zero issues
- 20-level Depth WebSocket (`wss://depth-api-feed.dhan.co/twentydepth?...`)
  — our Rust client streams **ATM ± 24 strikes × 4 indices**, zero issues
- REST APIs (Orders, Positions, Portfolio, Historical) — all working
- Python SDK (`dhanhq==2.2.0rc1`) 200-depth on same account — **works**

## What fails

- Rust 200-level depth WebSocket to `full-depth-api.dhan.co` —
  `Protocol(ResetWithoutClosingHandshake)` within seconds of
  `connect_async` returning Ok, in **every** configuration we tried.

## Specific questions

1. **What User-Agent string does the 200-depth WebSocket server require,
   if any?** Does the server actively filter based on `User-Agent`?
2. **What TLS handshake fingerprint** (JA3 or JA4) does your production
   Python SDK produce that our Rust `rustls` client does not?
3. **Does your server require** `Sec-WebSocket-Protocol: dhanhq` or any
   similar sub-protocol header that tungstenite defaults omit?
4. **Is there any WebSocket extension** (`permessage-deflate`,
   specific compression window) that your server requires?
5. **Does `full-depth-api.dhan.co` sit behind Cloudflare** with a
   WAF rule that flags non-Python-UA clients? If so, can our static
   IP `<IP>` be whitelisted on the WAF for the 200-depth endpoint?
6. **Can you enable TCP-level or WebSocket-handshake debug logging**
   on your server side for our Client ID `1106656882` during a
   scheduled test window, so we can see exactly what your server
   sees when our Rust client connects?

## What we will do once you answer

We will apply the minimum change to our Rust client (User-Agent,
sub-protocol header, WAF whitelist request — whatever you specify),
retest, and if successful close this ticket.

## Diagnostic tooling (reproducible by your team)

Our full test harness is open-source at:

https://github.com/SJParthi/tickvault/tree/claude/hardcode-dhan-api-7rgQC/crates/core/examples/depth_200_variants.rs

Your team can clone, set `DHAN_CLIENT_ID` + `DHAN_ACCESS_TOKEN` env
vars, run `cargo run --release --example depth_200_variants`, and
reproduce the failure against a test account of your choosing.

Thank you for your continued support. This ticket has been open
since 2026-04-10 — we are eager to close it and deploy 200-level
depth to production.

Best regards,
Parthiban S
Client ID: 1106656882
