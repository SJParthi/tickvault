# Groww API Support Communication Archive

Every technical email to Groww API support is drafted here as a markdown
file, committed to git, and shared with Groww as a **GitHub rendered
link**. This mirrors the proven `docs/dhan-support/` workflow:

- **One source of truth** — every incident, every request, every reply is in git
- **Perfect formatting** — GitHub renders tables, headers, code blocks natively
- **Zero Gmail layout issues** — Arial proportional font destroys ASCII tables
- **Full history** — every past communication is browsable, searchable, diff-able
- **Precise details by default** — the structure enforces instrument-level specificity

## Workflow

1. **New incident / reply needed?** Copy the structure of the newest
   dossier in this directory (or `docs/dhan-support/TEMPLATE.md`, adapting
   the identity block for Groww):
   ```bash
   cp docs/groww-support/2026-07-03-latency-floor-and-nats-eof.md \
      docs/groww-support/YYYY-MM-DD-<topic>.md
   ```

2. **Fill in every placeholder.** Run this to find them all:
   ```bash
   grep -n '<[A-Z_]' docs/groww-support/YYYY-MM-DD-<topic>.md
   ```

3. **Commit and push:**
   ```bash
   git add docs/groww-support/YYYY-MM-DD-<topic>.md
   git commit -m "docs(groww-support): <subject>"
   git push
   ```

4. **Copy the GitHub URL** from the pushed branch:
   ```
   https://github.com/SJParthi/tickvault/blob/<branch>/docs/groww-support/YYYY-MM-DD-<topic>.md
   ```

5. **Reply in Gmail with a SHORT message + the link.**
   Do NOT paste the markdown body into Gmail — the proportional font
   misaligns every table. Share the link, let GitHub render it.

## Non-negotiable details in every email

- **Groww identity** — Groww client/account identifier. This repo does
  NOT store one; use `<GROWW_CLIENT_ID — operator fill before sending>`
  placeholders and fill before sending. NEVER reuse the Dhan Client ID
  or UCC (see `docs/dhan-support/`) — those are Dhan credentials, not
  Groww identity.
- **Precise instrument labels** — the Groww trading symbol AND the Groww
  `exchange_token` AND our internal `security_id` for every instrument
  cited (e.g. `RELIANCE / NSE / exchange_token=2885 / security_id=2885`).
  Never generic ("some NSE stock").
- **Microsecond IST timestamps** — `YYYY-MM-DD HH:MM:SS.ffffff` format.
- **Verbatim log evidence** — fenced code blocks, raw lines, never
  reformatted. REDACT any token/JWT/credential material from pasted
  CloudWatch lines before sending — the `last_error=<detail>` field
  carries raw exception text.
- **What works vs what fails table** — same account, same minute; rules
  out account / token / host / network issues.
- **Numbered specific questions** — never "please help".
- **Diagnostic offer** — tcpdump, different instruments, reproduction
  windows, connection ids, etc.

## Feed transport note (Groww-specific)

The Groww live feed is **NATS-over-WebSocket + Protobuf** at
`wss://socket-api.groww.in`, consumed via the official `growwapi` Python
SDK 1.5.0 (nats-py 2.15.0). It is NOT a plain JSON WebSocket. The
sidecar's stderr log-string signatures (shipped to CloudWatch group
`/tickvault/prod/app`) to grep for in any incident:

```
groww sidecar: NATS error_cb -> <detail>
groww sidecar: NATS connection closed -> last_error=<detail>
groww sidecar: NATS disconnected -> last_error=<detail>
groww sidecar: NATS last_error -> <detail>
groww sidecar: force-close of NATS socket failed (<type>)
GROWW LIVE FEED REJECTED: <reason>          (edge-triggered; the token-stale form fires ONCE after 600s of continuous auth failure; a separate 60s heartbeat line uses `GROWW LIVE FEED STILL REJECTED:`)
```

Known behaviours to reference: the server can close the socket WITHOUT
raising into the SDK (swallowed "Authorization Violation" / bare EOF —
the blocking `feed.consume()` never returns); our stall watchdog
force-closes and reconnects on a 50ms→5s market-hours ladder. Daily
06:00 IST Groww token reset; the shared minter Lambda mints ~06:05 IST
(TickVault NEVER mints — see
`.claude/rules/project/groww-shared-token-minter-2026-07-02.md`).
Watchdog rule file:
`.claude/rules/project/feed-stall-watchdog-error-codes.md`
(FEED-STALL-01 / FEED-SUPERVISOR-01).

## File naming convention

```
YYYY-MM-DD-<short-topic>.md
```

## Archive

| File | Subject |
|---|---|
| `2026-07-03-latency-floor-and-nats-eof.md` | External snapshot-pipeline latency floor + NATS silent-EOF disconnects |
| `SUMMARY-2026-07-03.md` | One-page at-a-glance summary of the above |
| `sql/external-floor.sql` | Paste-ready QuestDB SQL producing the latency evidence tables |
