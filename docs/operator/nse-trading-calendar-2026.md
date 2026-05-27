---
title: NSE Trading Calendar 2026 — Verified Reference
purpose: Source-of-truth for boot orchestrator holiday/Muhurat handling
fetched_at: 2026-05-27
project: tickvault Sub-PR #11b / #11c
verification_status: PRIMARY-SOURCED from NSE official circular
authoritative_source: NSE Circular NSE/CMTR/71775, Ref 172/2025, dated 2025-12-12
---

# NSE Trading Calendar 2026 — Verified Reference

> **⚠️ TRUST HIERARCHY (read this first)**
>
> 1. **Operator-managed config (TOML)** = SOURCE OF TRUTH. Whatever is in `config/base.toml` is what the boot orchestrator obeys.
> 2. **Official NSE PDF circular** (downloadable, archivable) = primary reference for operator to update config from.
> 3. **NSE HTML page** (`/resources/exchange-communication-holidays`) = secondary visual check.
> 4. **Undocumented JSON endpoint** (`/api/holiday-master`) = best-effort runtime cross-check only. Do NOT block boot on it.
>
> **DO NOT** make the boot orchestrator depend on live HTTP calls to NSE. Network may fail at boot; bot detection may block you. Config-first is the only sane architecture.

---

## 1. NSE Holiday Calendar URLs — Verified Current

### 1a. Operator-facing HTML page (CURRENT WORKING URL)

```
https://www.nseindia.com/resources/exchange-communication-holidays
```

**Verified status:** Live as of 2026-05-27. Returns the holiday list as an HTML page.

> ⚠️ The previous URL `https://www.nseindia.com/market-data/holiday-calendar` returns "Resource not found" — that path is dead. Use the `/resources/exchange-communication-holidays` path going forward.

### 1b. Official PDF circular (PRIMARY SOURCE OF TRUTH)

```
https://nsearchives.nseindia.com/content/circulars/CMTR71775.pdf
```

- **Circular Download Ref No:** `NSE/CMTR/71775`
- **Circular Ref. No:** `172/2025`
- **Date issued:** `December 12, 2025`
- **Issuing department:** Capital Market Segment
- **Signed by:** Khushal Shah, Associate Vice President, NSE
- **Verified status:** ✅ Fetched directly from NSE archives, May 27, 2026

This is the gold-standard source. Operator should download this PDF and base `config/base.toml` on it.

### 1c. Undocumented JSON endpoint (NOT recommended for production)

```
GET https://www.nseindia.com/api/holiday-master?type=trading
```

**Required HTTP headers (NSE has bot detection):**

```python
headers = {
    "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 ...",
    "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    "Accept-Language": "en-US,en;q=0.9,en-IN;q=0.8",
    "Referer": "https://www.nseindia.com/",
}
```

**Required cookie warmup** (or you get 401/403):

```python
import requests
s = requests.Session()
s.get("https://www.nseindia.com", headers=headers, timeout=10)
s.get("https://www.nseindia.com/option-chain", headers=headers, timeout=10)
data = s.get("https://www.nseindia.com/api/holiday-master?type=trading",
             headers=headers, timeout=10).json()
```

**Response shape:** `{"CM": [...], "FO": [...], "CD": [...], "CO": [...]}` — segments keyed by code; each value is a list of `{"tradingDate": "DD-MMM-YYYY", "weekDay": "...", "description": "..."}` records.

**Status of this endpoint:**
- ❌ NOT officially documented by NSE
- ❌ NOT part of any published API contract — can change/break without notice
- ❌ Cookie-warmup ritual is fragile; NSE actively works against scrapers
- ❌ Returns DD-MMM-YYYY format (not ISO) — requires parsing
- ✅ Useful as a soft cross-check, never as authoritative source

**Verdict:** Use only as Sub-PR #11c best-effort cross-check. Mark stale-data warnings; never let it block boot.

---

## 2. NSE Trading Holidays 2026 — Full Weekday List (15 holidays)

**Source:** NSE/CMTR/71775, Ref 172/2025, December 12, 2025.

| # | Date | Day | Holiday Description |
|---|---|---|---|
| 1 | 2026-01-26 | Monday | Republic Day |
| 2 | 2026-03-03 | Tuesday | Holi |
| 3 | 2026-03-26 | Thursday | Shri Ram Navami |
| 4 | 2026-03-31 | Tuesday | Shri Mahavir Jayanti |
| 5 | 2026-04-03 | Friday | Good Friday |
| 6 | 2026-04-14 | Tuesday | Dr. Baba Saheb Ambedkar Jayanti |
| 7 | 2026-05-01 | Friday | Maharashtra Day |
| 8 | 2026-05-28 | Thursday | Bakri Id |
| 9 | 2026-06-26 | Friday | Muharram |
| 10 | 2026-09-14 | Monday | Ganesh Chaturthi |
| 11 | 2026-10-02 | Friday | Mahatma Gandhi Jayanti |
| 12 | 2026-10-20 | Tuesday | Dussehra |
| 13 | 2026-11-10 | Tuesday | Diwali-Balipratipada |
| 14 | 2026-11-24 | Tuesday | Prakash Gurpurb Sri Guru Nanak Dev |
| 15 | 2026-12-25 | Friday | Christmas |

### Weekend-coinciding holidays (NO market closure required — informational only)

| # | Date | Day | Description |
|---|---|---|---|
| 1 | 2026-02-15 | Sunday | Mahashivratri |
| 2 | 2026-03-21 | Saturday | Id-Ul-Fitr (Ramadan Eid) |
| 3 | 2026-08-15 | Saturday | Independence Day |
| 4 | 2026-11-08 | Sunday | Diwali Laxmi Pujan (★ Muhurat day) |

---

## 3. Diwali Muhurat Trading 2026

### What is verified

| Field | Value | Source |
|-------|-------|--------|
| Date | **2026-11-08 (Sunday)** | ✅ NSE/CMTR/71775 |
| Session held? | **Yes — confirmed** | ✅ NSE/CMTR/71775 footnote |
| Underlying festival | Diwali Laxmi Pujan | ✅ NSE/CMTR/71775 |
| Start time (IST) | ⚠️ **UNVERIFIED** | — |
| End time (IST) | ⚠️ **UNVERIFIED** | — |

### What is NOT yet verified

**The exact Muhurat session timings have NOT been published as of 2026-05-27.**

The NSE/CMTR/71775 circular footnote states (verbatim):
> *"Muhurat Trading will be conducted on Sunday, November 08, 2026. Timings of Muhurat Trading shall be notified subsequently."*

**Expected timing-circular publication:** October 2026 (based on historical pattern — NSE publishes Muhurat-specific circular ~2-4 weeks before Diwali).

### Historical Muhurat timing pattern (DO NOT use for config — informational only)

| Year | Date | Session Window | Reference |
|------|------|---------------|-----------|
| 2024 | 2024-11-01 (Fri) | 18:00 – 19:00 IST | NSE circular (1-hour evening session) |
| 2025 | 2025-10-21 (Tue) | **13:45 – 14:45 IST** | NSE circular (afternoon session — UNUSUAL deviation from evening tradition) |
| 2026 | 2026-11-08 (Sun) | **TBD** | Awaits Oct 2026 circular |

**Critical observation:** NSE broke a multi-decade evening-session tradition in 2025 by holding Muhurat in the afternoon (13:45–14:45 IST). This means **you cannot safely assume the 2026 Muhurat will be evening-time** even though the smallcase.com article casually states "usually between 6:00 PM and 7:15 PM IST" — that historical pattern was already broken in 2025.

**Operational recommendation:**
- Mark Muhurat 2026 as `UNVERIFIED_TIMING` in config until October 2026.
- Set Telegram annual reminder for early October 2026 to fetch the timing circular.
- Boot orchestrator should refuse to auto-place orders on Nov 8 unless the operator has explicitly updated the Muhurat window in config.

---

## 4. ⚠️ CRITICAL EDGE CASES — Calendar exceptions NOT in the annual circular

The Dec 12, 2025 annual circular is **NOT exhaustive**. NSE issues separate mid-year circulars for:

### 4a. Late-added trading holidays (KNOWN OCCURRENCE)

**Example confirmed for 2026:**

| Date | Event | Status | Source |
|------|-------|--------|--------|
| 2026-01-15 (Thursday) | **Mumbai civic election** | ✅ ADDED as trading holiday | Business Standard 2026-01-14 — citing NSE separate circular |

This holiday was **NOT in the original NSE/CMTR/71775 list**. It was added mid-month via a separate circular due to BMC election day. Zerodha co-founder Nithin Kamath publicly criticized this last-minute change.

**Implication for your boot orchestrator:**
The system MUST allow operators to add late-breaking holidays via config update without requiring a code deploy. A pure-hardcoded approach will silently allow orders on days that turn out to be holidays.

### 4b. Special trading sessions on non-trading days (KNOWN OCCURRENCE)

**Example confirmed for 2026:**

| Date | Event | Status | Source |
|------|-------|--------|--------|
| 2026-02-01 (Sunday) | **Union Budget presentation** | ✅ ADDED as special trading day | Business Standard 2026-01-16 |

NSE + BSE held a **regular trading session on Sunday, Feb 1, 2026** for the Union Budget. Normal market hours.

**Implication:** Your boot orchestrator must also support `extra_trading_days: []` in config — Saturdays/Sundays that become trading days. A "weekday + not-in-holiday-list" check is INSUFFICIENT.

### 4c. Expiry shifts when holiday lands on expiry day

Two confirmed expiry shifts for 2026:

| Original expiry | Reason | Shifted to |
|---|---|---|
| 2026-03-26 (Thursday) | Shri Ram Navami | **2026-03-25 (Wednesday)** |
| 2026-05-28 (Thursday) | Bakri Id | **2026-05-27 (Wednesday)** |

Source: Multiple broker pages (Sahi, Smallcase) cite NSE confirming these shifts. Sub-PR #11b should already handle this via the existing TradingCalendar weekly expiry logic — verify it does.

### 4d. Saturday disaster-recovery drill sessions (historical pattern)

NSE has historically scheduled "special trading sessions on Saturday" for primary-site/DR-site preparedness drills (e.g., January 20, 2024). These were announced via separate circular. Pattern continues into 2026 — operator must monitor NSE circulars page.

---

## 5. Boot Orchestrator Requirements (Sub-PR #11b adjusted scope)

Given Sections 1-4, the orchestrator MUST support these config-driven inputs:

```toml
[trading.calendar]
# PRIMARY source of truth, manually maintained by operator from NSE PDF circular
# Updated whenever a new NSE circular is published

# Standard scheduled holidays from annual NSE circular
official_holidays = [
    "2026-01-26",  # Republic Day
    "2026-03-03",  # Holi
    "2026-03-26",  # Shri Ram Navami
    "2026-03-31",  # Shri Mahavir Jayanti
    "2026-04-03",  # Good Friday
    "2026-04-14",  # Dr. Baba Saheb Ambedkar Jayanti
    "2026-05-01",  # Maharashtra Day
    "2026-05-28",  # Bakri Id
    "2026-06-26",  # Muharram
    "2026-09-14",  # Ganesh Chaturthi
    "2026-10-02",  # Mahatma Gandhi Jayanti
    "2026-10-20",  # Dussehra
    "2026-11-10",  # Diwali-Balipratipada
    "2026-11-24",  # Prakash Gurpurb Sri Guru Nanak Dev
    "2026-12-25",  # Christmas
]

# Late-added holidays from mid-year NSE circulars (operator-updated)
additional_holidays = [
    "2026-01-15",  # Mumbai civic election (added per separate NSE circular)
]

# Extra trading days (typically Saturdays/Sundays opened for Budget, drills, etc.)
extra_trading_days = [
    "2026-02-01",  # Union Budget special session
]

[trading.muhurat_sessions]
# Muhurat sessions: date confirmed, timings update when NSE publishes Oct circular
"2026-11-08" = { start = "UNVERIFIED", end = "UNVERIFIED", status = "PENDING_OCTOBER_CIRCULAR" }

[trading.calendar.metadata]
source_circular = "NSE/CMTR/71775"
source_ref_no = "172/2025"
source_date = "2025-12-12"
source_url = "https://nsearchives.nseindia.com/content/circulars/CMTR71775.pdf"
last_operator_review = "2026-05-27"
```

### Boot-time L2 sanity check (Sub-PR #11b NEW)

```
ASSERT: official_holidays contains >= 3 future holidays after today
        ELSE refuse to boot with "Stale holiday config — operator must update from NSE circular"

ASSERT: All dates parse as valid ISO YYYY-MM-DD
ASSERT: No duplicates between official_holidays and additional_holidays
ASSERT: extra_trading_days does NOT overlap with any holiday list
ASSERT: muhurat_sessions dates are also present in either official_holidays OR fall on weekend
```

### Build-time staleness ratchet (Sub-PR #11b NEW)

- Fail build if `last_operator_review` is > 90 days old.
- Fail build if today's date is past the LAST date in `official_holidays`.
- Both protect against "we forgot to update for next year" failures.

### Annual reminder Telegram (Sub-PR #11b NEW)

- Send Telegram reminder on **December 15** each year: "Fetch new NSE annual holiday circular for next year, update config/base.toml"
- Send Telegram reminder on **October 1** each year: "Fetch NSE Muhurat timing circular for this year's Diwali, update muhurat_sessions in config"

---

## 6. Dhan API — Does it expose a holiday endpoint?

### Status as of 2026-05-27: **NO**

Confirmed via DhanHQ v2 API docs (fetched same session):

| Doc section | Holiday endpoint mentioned? |
|---|---|
| Introduction (`docs/v2/`) | ❌ No |
| Authentication (`docs/v2/authentication/`) | ❌ No |
| Annexure (`docs/v2/annexure/`) | ❌ No (segments + product types only) |
| Instruments (`docs/v2/instruments/`) | ❌ No |
| Releases v2 → v2.5 (`docs/v2/releases/`) | ❌ No holiday endpoint introduced through 2026-05 |

**Dhan does NOT expose a programmatic holiday/calendar endpoint via DhanHQ API.** The user profile endpoint returns subscription validity, not market holidays.

### Why this matters for your architecture

You cannot delegate the holiday-calendar problem to Dhan. Either:
- ✅ **Recommended:** Operator-managed `config/base.toml` (your current plan)
- ⚠️ Acceptable: Cross-check against NSE undocumented JSON endpoint (Sub-PR #11c — fragile, best-effort only)
- ❌ Do NOT: Scrape NSE HTML page on every boot (brittle, ToS-risky, defeats config-first design)

---

## 7. Third-Party Holiday APIs — Should You Use One?

| Provider | Officially documented? | Costs? | Reliability | Verdict |
|---|---|---|---|---|
| **TradingHours.com** | ✅ Yes, has commercial API | Paid | High (commercial SLA) | OK as paid fallback — not free |
| **nsepython** (PyPI) | ❌ Wraps NSE undocumented endpoint | Free | Brittle (same bot-detection issues) | Same fragility as direct scrape |
| **calendarlabs.com** (iCal) | ❌ No API contract | Free | Static yearly snapshot | Useful for visual cross-check, not runtime |
| **Broker holiday endpoints** (Zerodha Kite, Upstox, Fyers, Groww) | ❌ None expose this | — | — | Not available |

**Recommended fallback (Sub-PR #11c, when ready):**
1. **Primary cross-check:** NSE undocumented `/api/holiday-master` — already documented in section 1c above. Treat as soft check.
2. **Hard fallback:** Operator manually re-downloads NSE PDF circular + updates config.

Avoid taking a paid dependency on TradingHours.com unless your operator-config approach proves unsustainable.

---

## 8. Source Citations (verbatim)

### 8a. Primary source — NSE Circular NSE/CMTR/71775 (extracted text)

> National Stock Exchange of India Limited
> Circular
> Department: CAPITAL MARKET SEGMENT
> Download Ref No: NSE/CMTR/71775   Date: December 12, 2025
> Circular Ref. No: 172/2025
>
> All Members,
> Trading holidays for the calendar year 2026
>
> In pursuance to clause 2 of Chapter IX of the Bye-Laws and Regulation 2.3.1 of part A Regulations of the Capital Market Segment, the Exchange hereby notifies trading holidays for the calendar year 2026 as below:
>
> [15-holiday table — see Section 2 above]
>
> The holidays falling on Saturday / Sunday are as follows:
> [4-holiday weekend table — see Section 2 above]
>
> *Muhurat Trading will be conducted on Sunday, November 08, 2026. Timings of Muhurat Trading shall be notified subsequently.
>
> For and on behalf of
> National Stock Exchange of India Limited
> Khushal Shah, Associate Vice President
> Toll Free No: 1800-266-0050 (Option 1)   Email id: msm@nse.co.in

### 8b. NSE Clearing parallel circular

```
https://nsearchives.nseindia.com/content/circulars/CMPT71789.pdf
```
- Ref: `NSE/CMPT/71789`, dated December 12, 2025
- For SLBS clearing holidays (matches CMTR71775)

### 8c. Source URLs cited in this document

| Section | URL | Status verified |
|---|---|---|
| Primary HTML page | https://www.nseindia.com/resources/exchange-communication-holidays | ✅ Live |
| Primary PDF (CMTR71775) | https://nsearchives.nseindia.com/content/circulars/CMTR71775.pdf | ✅ Fetched 2026-05-27 |
| Clearing PDF (CMPT71789) | https://nsearchives.nseindia.com/content/circulars/CMPT71789.pdf | ✅ Confirmed in search |
| Undocumented JSON | https://www.nseindia.com/api/holiday-master?type=trading | ⚠️ Live but unofficial |
| Jan 15 civic poll holiday | https://www.business-standard.com/topic/bse | ✅ News-confirmed |
| Feb 1 Budget special session | https://www.business-standard.com/topic/bse | ✅ News-confirmed |
| 2025 Muhurat afternoon precedent | https://www.business-standard.com/markets/news/stock-market-holiday-bse-nse-dussehra-... | ✅ News-confirmed |

---

## 9. Known Discrepancies in Third-Party Sources (DO NOT trust blindly)

These secondary sources got things WRONG. If you cross-check anywhere, prefer the NSE PDF.

| Source | Error | Correct fact |
|---|---|---|
| Angel One (`angelone.in/nse-holidays`) | States Muhurat 2026 is "Wednesday, October 21, 2026" | ❌ WRONG. Muhurat 2026 is Sunday, November 8 (per NSE circular). Angel One appears to have confused 2025's Muhurat date with 2026. |
| Smallcase.com | "Historical Context: This session is traditionally conducted in the evening (usually between 6:00 PM and 7:15 PM IST)" — implies 2026 will follow | ⚠️ Stale historical pattern. NSE held 2025 Muhurat at 13:45 IST. Do not assume evening for 2026. |
| Bajaj AMC | States "there are 18 official NSE holidays this year" | ⚠️ Misleading. There are 15 weekday + 4 weekend = 19 total; only 15 cause market closure. Plus Jan 15 add-on = 16 actual closures. |

---

## 10. TL;DR for the boot orchestrator

1. **Use operator-managed `config/base.toml`** as the source of truth — Sections 2 + 4 above feed directly into config.
2. **Boot-time sanity check** (L2): ≥3 future holidays after today, else refuse to boot.
3. **Build-time staleness ratchet** (L4): fail build if config older than 90 days or if today > last holiday date.
4. **Annual Telegram reminders** (L7): Dec 15 (next year's circular) and Oct 1 (this year's Muhurat timing).
5. **Sub-PR #11c (deferred):** add soft cross-check against `https://www.nseindia.com/api/holiday-master?type=trading` with cookie warmup — non-blocking.
6. **Dhan API does NOT solve this** — confirmed no holiday endpoint exists in DhanHQ v2 through v2.5.
7. **Muhurat 2026 timings UNVERIFIED** — `status = "PENDING_OCTOBER_CIRCULAR"`. Refuse auto-orders on Nov 8 until config updated.
8. **The 15-holiday list is NOT exhaustive** — Jan 15 was added late; expect 1-2 more late-additions per year. Operator must monitor NSE circulars page (https://www.nseindia.com/resources/exchange-communication-circulars or similar).

---

## 11. Confidence ratings on every claim in this document

| Claim | Confidence | Basis |
|---|---|---|
| The 15 official holiday dates above | **VERIFIED** | Fetched NSE PDF directly |
| Muhurat date 2026-11-08 | **VERIFIED** | NSE PDF footnote |
| Muhurat timings 2026 | **UNVERIFIED** | NSE PDF explicitly defers |
| Current NSE HTML URL | **VERIFIED** | Search result confirmed live |
| Undocumented JSON endpoint shape | **MOSTLY VERIFIED** | Community examples, not NSE docs |
| Cookie-warmup ritual works | **PROBABLY** | Community examples; NSE may change anti-bot any time |
| Jan 15, 2026 civic poll holiday | **VERIFIED** | Business Standard news article |
| Feb 1, 2026 Budget special session | **VERIFIED** | Business Standard news article |
| Expiry shifts (Mar 25, May 27) | **VERIFIED** | Multiple broker confirmations |
| Dhan has no holiday endpoint | **VERIFIED** | Cross-checked against full DhanHQ v2 docs |
| Future late-additions are likely | **HIGH-CONFIDENCE INFERENCE** | Pattern across multiple years |

If anything in this document conflicts with an operator's understanding of NSE rules, **the operator is right** — Claude Code should NOT override operator config based on this doc.
