<!--
=============================================================================
DHAN SUPPORT EMAIL TEMPLATE
=============================================================================
Copy this file to: docs/dhan-support/YYYY-MM-DD-<ticket>-<topic>.md
Fill in every <PLACEHOLDER>. Delete sections that don't apply.
Share the GitHub link in the Gmail reply — do NOT copy-paste plain text.

Why this format:
- GitHub renders tables, headers, and code blocks natively.
- Gmail's Arial proportional font destroys ASCII alignment.
- The operator shares one short reply + one link.
- Dhan engineering gets a permanent, searchable, version-controlled record.
=============================================================================
-->

# <Subject Line> — Ticket #<NUMBER>

**To:** apihelp@dhan.co
**From:** <OPERATOR_EMAIL>
**Subject:** Re: Ticket #<NUMBER> — <ONE-LINE SUMMARY>
**Date:** YYYY-MM-DD

---

Hi <SUPPORT_AGENT_NAME>,

<One-line thank you / context recap. 1-2 sentences max.>

| Field | Value |
|---|---|
| Client ID | `<CLIENT_ID>` |
| Name | <FULL NAME> |
| UCC | `<UCC>` |
| Date of incident | <DATE IST> |
| Time of incident | <HH:MM:SS IST> |

---

## What we tested / observed

<One paragraph describing the exact scenario — what was subscribed, how
many connections, what expiry, what strike. Be precise. No vagueness.>

| Contract | SecurityId | Timestamp (IST) | Outcome |
|---|---|---|---|
| `<PRECISE-CONTRACT-LABEL>` | `<SID>` | `YYYY-MM-DD HH:MM:SS.ffffff` | <reset / subscribed / ...> |
| `<PRECISE-CONTRACT-LABEL>` | `<SID>` | `YYYY-MM-DD HH:MM:SS.ffffff` | <reset / subscribed / ...> |

**URL used:**

```
<EXACT WSS URL>
```

**Request JSON:**

```json
<EXACT REQUEST BODY>
```

**Error / response captured:**

```
<EXACT ERROR OR RESPONSE>
```

---

## Key observation

<What makes this incident diagnostic — e.g. "data flowed for 3s before the
reset", "only this endpoint fails while others work", "only certain
expiries fail". One or two short paragraphs. Data-driven.>

| Timestamp (IST) | Event | Value |
|---|---|---|
| `<HH:MM:SS.fff>` | <event> | <value> |

---

## What works on the same account / same minute

Rule out account / token / network issues by proving other WS types work.

| WebSocket | Endpoint | Status | Evidence |
|---|---|---|---|
| Live Market Feed | `api-feed.dhan.co` | <Streaming / Failing> | <counts / log refs> |
| 20-level depth | `depth-api-feed.dhan.co/twentydepth` | <Streaming / Failing> | <counts / log refs> |
| 200-level depth | `full-depth-api.dhan.co/twohundreddepth` | <Streaming / Failing> | <counts / log refs> |
| Order Update | `api-order-update.dhan.co` | <Streaming / Failing> | <counts / log refs> |

---

## Verbatim log lines (for grep)

```json
<PASTE RAW JSON LOG LINES VERBATIM — DO NOT REFORMAT>
```

---

## Requests to Dhan engineering

1. **<Specific question 1>** — Please check <exact thing at exact time>.
2. **<Specific question 2>** — Is <exact feature flag / cap / plan item> active on account `<CLIENT_ID>`?
3. **<Specific question 3>** — Can you share a working reference script for <exact API>?
4. **<Specific question 4>** — Can engineering enable <specific diagnostic logging> on our clientId for <time window>?
5. **<Specific question 5>** — <anything else, one per line>

---

We are happy to run any diagnostic tests you need — <list: different SIDs,
different times, tcpdump, secondary IP, etc.>. This is blocking
<specific feature / revenue / trading path> and we need to move forward.

Thank you,
**<FULL NAME>**
Client ID: `<CLIENT_ID>`

<!--
=============================================================================
OPERATOR INSTRUCTIONS (DO NOT INCLUDE IN THE EMAIL)
=============================================================================
1. Fill in every <PLACEHOLDER> above. Use `grep '<' THIS-FILE.md` to find them.
2. Commit + push: git add docs/dhan-support/<file>.md && git commit -m "docs(dhan-support): <subject>" && git push
3. Copy the GitHub URL: https://github.com/SJParthi/tickvault/blob/<branch>/docs/dhan-support/<file>.md
4. In Gmail, reply with a SHORT message and the link:
     "Hi <agent>, full details with server correlation data here: <link>. Thank you, Parthiban"
5. Never paste the markdown body into Gmail directly — proportional font breaks tables.
=============================================================================
-->
