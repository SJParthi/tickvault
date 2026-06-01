# Dhan HQ Bot — chat prompts for the Live Market Feed 429 issue (2026-06-01)

Paste these into the **DhanHQ Chatbot** (the "HQ Bot" on the docs site) **one at a time**.
They carry the precise facts so the bot (or the human it escalates to) can answer without back-and-forth.

> Companion: the full support email is
> `docs/dhan-support/2026-06-01-live-feed-429-from-cloud-ip.md`
> (share that GitHub link with apihelp@dhan.co).

**Account context (paste once at the top if the bot asks):**
> Client ID `1106656882`, UCC `NWXF17021Q`. Issue: Live Market Feed `wss://api-feed.dhan.co`.

---

### Prompt 1 — the rate limit
```
What is the connection rate limit for the Live Market Feed WebSocket
(api-feed.dhan.co) per client ID? How many new connection attempts per
minute before I get HTTP 429 Too Many Requests?
```

### Prompt 2 — the cooldown + scope
```
If the Live Market Feed returns "429 Too Many Requests", how long is the
cooldown before I can connect again, and is that limit counted per client
ID or per source IP address? I rotated my server IP 3 times and still got
429, which suggests it is per client ID — please confirm.
```

### Prompt 3 — cloud vs home IP (the key evidence)
```
Same access token, same account (client ID 1106656882): my home BSNL
static IP 59.92.114.17 connects to the Live Market Feed and streams 331
instruments fine, but my AWS server IPs (13.205.212.203 / 13.234.39.4 /
65.1.58.124) all get HTTP 429. Does the Live Market Feed treat cloud /
datacenter IPs differently from home IPs? Is there any block on my account
right now?
```

### Prompt 4 — static IP scope (confirm on record)
```
Is static IP whitelisting required for the Live Market Feed (market data),
or only for Order placement APIs? My understanding from the docs is it is
ONLY for orders — please confirm the market feed needs no IP whitelisting.
```

---

**What a good answer looks like:** the bot/agent confirms (a) the feed's per-minute connection cap, (b) the 429 cooldown duration, (c) whether it's per-account or per-IP, and (d) that the feed needs no IP whitelist. With (a)+(b) we size our reconnect backoff exactly; with (c) we know whether the AWS box can ever work or whether we need a home/static-IP egress.
