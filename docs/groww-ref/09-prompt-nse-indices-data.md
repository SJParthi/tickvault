# CLAUDE CODE SESSION PROMPT — NSE Indices Data (all-indices CSV + per-index history)

> Paste this whole file into a fresh Claude Code session as the task brief. It contains the goal, the verified endpoints, the index taxonomy, column layouts, a fetcher skeleton, and the gotchas.
> **Compiled & verified:** 13 June 2026.

---

## ROLE / TASK

You are building an NSE **index data** ingestion module for a personal trading/backtesting system (repo: tickvault). Implement two capabilities:

1. **Daily all-indices snapshot** — one CSV per trading day containing **every published NSE index** (N indices in a single file), via NSE's `ind_close_all` report.
2. **Per-index long history** — full OHLC time series for any single index (e.g. NIFTY 50, NIFTY BANK), for multi-year backfills.

Follow the verified endpoints, column maps, and gotchas below. Do **not** invent URLs. Where something is marked ⚠️ verify-first, smoke-test it before relying on it.

---

## KEY INSIGHT — "N indices from one CSV"

The `ind_close_all_<DDMMYYYY>.csv` file is a **single daily file that holds ALL currently-published NSE indices as rows** (broad-market + sectoral + thematic + strategy). So you get N indices from ONE download — you do **not** loop per index for the daily snapshot.

- **Do NOT hardcode the index list.** The authoritative, always-current set is the **`Index Name` column of the file itself**. Read the file, then `df["Index Name"].unique()` → that's your live N.
- NSE maintains **~421 indices** under the NIFTY brand (as of Jan 2026), but not all appear in the daily EOD file; the file carries the published/active EOD set (typically 100+). Treat the file as ground truth for "what exists today."

---

## ROUTE A — Daily all-indices snapshot (`ind_close_all`)

**URL:**
```
https://nsearchives.nseindia.com/content/indices/ind_close_all_<DDMMYYYY>.csv
```
- Plain `.csv` (NOT zipped). Date token **`DDMMYYYY`** (e.g. `01052026`). Example:
  `https://nsearchives.nseindia.com/content/indices/ind_close_all_01052026.csv`
- One row per index for that trading day. ⚠️ verify-first (NSE's long-stable indices path — smoke-test).

**Columns:**
```
Index Name, Index Date, Open Index Value, High Index Value, Low Index Value,
Closing Index Value, Points Change, Change(%), Volume, Turnover (Rs. Cr.),
P/E, P/B, Div Yield
```
(Some vintages also carry 52-week high/low. Mapping confirmed via real-file usage: `Index Name`=ticker, `Index Date`=date, `Open Index`=open, `High Index`=high, `Low Index`=low, `Closing Index`=close.)

- **`Volume` is 0 for indices** — indices are not traded instruments, so volume/OI are not meaningful here. Expected, not a bug.
- Date token differs from the bhavcopy UDiFF token (`YYYYMMDD`) — indices use `DDMMYYYY`. Don't mix them up.
- Weekend/holiday → HTTP 404 (skip).

---

## ROUTE B — Per-index long history

For a single index across many years (backfill), use one of:

### B1. NSE Indices official backpage (POST) — full history, free
```
POST https://www.niftyindices.com/Backpage.aspx/getHistoricaldatatabletoString
```
- Prime cookies first: `GET https://www.niftyindices.com` with a browser UA.
- JSON payload: `{"name": "NIFTY 50", "startDate": "01-Jan-2000", "endDate": "13-Jun-2026"}`
  - `name` must match the index's official name (e.g. `NIFTY 50`, `NIFTY BANK`, `NIFTY IT`).
  - Dates in `DD-Mmm-YYYY` format.
- Returns historical OHLC rows (as a JSON-wrapped string → parse `d` then `.json`).
- ✅ endpoint verified this session. Anti-bot: needs the cookie prime + browser headers + `Referer: https://www.niftyindices.com`.

### B2. NSE site API (alternative) — ⚠️ verify-first
```
https://www.nseindia.com/api/historical/indicesHistory?indexType=<INDEX>&from=<DD-MM-YYYY>&to=<DD-MM-YYYY>
```
- `indexType` URL-encoded (e.g. `NIFTY%2050`). Same NSE cookie+header dance as bhavcopy. Returns JSON. Smoke-test param names — NSE rotates these.

### B3. Symbol-based (simplest, very reliable for major indices)
Use `yfinance` for the big/liquid indices:
| Index | Yahoo symbol |
| --- | --- |
| NIFTY 50 | `^NSEI` ✅ |
| NIFTY 500 | `^CRSLDX` ✅ |
| NIFTY BANK | `^NSEBANK` |
| BSE SENSEX | `^BSESN` ✅ |
| India VIX | `^INDIAVIX` |
```python
import yfinance as yf
nifty = yf.download("^NSEI", start="2015-01-01", end="2026-06-13", interval="1d")
```
- ✅ `^NSEI`, `^CRSLDX`, `^BSESN` verified. Others smoke-test the symbol once. Best for major indices; B1 covers the long tail of sectoral/thematic indices that Yahoo may not carry.

---

## INDEX TAXONOMY (for filtering the daily file / picking `name` for Route B)

> Authoritative live list = the file's `Index Name` column. This taxonomy is for grouping/filtering. NSE buckets indices as **Broad-Based, Sectoral, Thematic, Strategy (Smart-Beta)**.

**Broad-based (market-cap):**
NIFTY 50, NIFTY Next 50, NIFTY 100, NIFTY 200, NIFTY 500, NIFTY Total Market, NIFTY500 Multicap 50:25:25, NIFTY LargeMidcap 250, NIFTY MidSmallcap 400, NIFTY Midcap 150, NIFTY Midcap 100, NIFTY Midcap 50, NIFTY Midcap Select, NIFTY Smallcap 250, NIFTY Smallcap 100, NIFTY Smallcap 50, NIFTY Microcap 250.

**Sectoral (the 14 NSE sectors):**
NIFTY Auto, NIFTY Bank, NIFTY Chemicals, NIFTY Financial Services, NIFTY Financial Services 25/50, NIFTY Financial Services Ex-Bank, NIFTY FMCG, NIFTY Healthcare, NIFTY IT, NIFTY Media, NIFTY Metal, NIFTY Pharma, NIFTY Private Bank, NIFTY PSU Bank. (Plus NIFTY Consumer Durables, NIFTY Oil & Gas, NIFTY Realty appear in sectoral/industry groupings.)

**Thematic (examples):**
NIFTY Commodities, NIFTY CPSE, NIFTY Energy, NIFTY India Consumption, NIFTY India Defence, NIFTY India Digital, NIFTY India Manufacturing, NIFTY Infrastructure, NIFTY MNC, NIFTY PSE, NIFTY Services Sector, NIFTY Shariah 25, NIFTY EV & New Age Automotive, NIFTY Mobility, NIFTY Housing, NIFTY Core Housing, NIFTY Non-cyclical Consumer, plus group indices (Tata Group, Mahindra Group, Aditya Birla Group).

**Strategy / Smart-Beta (examples):**
NIFTY50 Equal Weight, NIFTY100 Equal Weight, NIFTY100 Low Volatility 30, NIFTY Low Volatility 50, NIFTY100 Alpha 30, NIFTY Alpha 50, NIFTY200 Quality 30, NIFTY100 Quality 30, NIFTY Midcap 150 Quality 50, NIFTY Dividend Opportunities 50, NIFTY High Beta 50, NIFTY 500 Value 50, NIFTY100 Liquid 15, NIFTY Midcap Liquid 15, plus Momentum/Quality factor variants.

**Also published:** India VIX (volatility index).

> ~421 total under the NIFTY brand as of Jan 2026; the above is the high-use core, not the full enumeration. For the complete current set, read the daily file's `Index Name` column (Route A).

---

## IMPLEMENTATION SKELETON

```python
import requests, io, time
from datetime import date, timedelta
import pandas as pd

NSE_HEADERS = {
    "User-Agent": ("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) "
                   "AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36"),
    "Accept": "*/*", "Accept-Language": "en-US,en;q=0.9",
    "Referer": "https://www.nseindia.com/all-reports",
}

# ---------- ROUTE A: daily all-indices snapshot ----------
def nse_session():
    s = requests.Session(); s.headers.update(NSE_HEADERS)
    s.get("https://www.nseindia.com", timeout=10)            # prime cookies (mandatory)
    return s

def fetch_all_indices(d: date, sess=None):
    sess = sess or nse_session()
    url = f"https://nsearchives.nseindia.com/content/indices/ind_close_all_{d:%d%m%Y}.csv"
    r = sess.get(url, timeout=20)
    if r.status_code == 404:
        return None                                          # weekend/holiday
    r.raise_for_status()
    df = pd.read_csv(io.BytesIO(r.content))
    df.columns = [c.strip() for c in df.columns]
    return df

def list_live_indices(d: date):
    df = fetch_all_indices(d)
    return sorted(df["Index Name"].unique()) if df is not None else []

def pull_all_indices_range(start: date, end: date) -> pd.DataFrame:
    sess, frames, d = nse_session(), [], start
    while d <= end:
        if d.weekday() < 5:
            try:
                df = fetch_all_indices(d, sess)
                if df is not None:
                    frames.append(df)
            except Exception as e:
                print(f"skip {d}: {e}")
            time.sleep(0.4)                                  # be polite -> avoid bans
        d += timedelta(days=1)
    return pd.concat(frames, ignore_index=True) if frames else pd.DataFrame()

# ---------- ROUTE B1: per-index long history (niftyindices) ----------
def index_history(name: str, start="01-Jan-2010", end="13-Jun-2026"):
    base = "https://www.niftyindices.com"
    url = base + "/Backpage.aspx/getHistoricaldatatabletoString"
    h = {**NSE_HEADERS, "Referer": base, "Content-Type": "application/json"}
    s = requests.Session(); s.get(base, headers=h, timeout=10)     # prime cookies
    payload = {"name": name, "startDate": start, "endDate": end}
    r = s.post(url, headers=h, json=payload, timeout=30)
    r.raise_for_status()
    import json
    inner = json.loads(r.json()["d"])                              # JSON-wrapped string
    return pd.DataFrame(inner)

# ---------- ROUTE B3: symbol-based (major indices) ----------
def index_history_yf(symbol="^NSEI", start="2015-01-01", end="2026-06-13"):
    import yfinance as yf
    return yf.download(symbol, start=start, end=end, interval="1d")
```

---

## GOTCHAS (assume all of these)

1. **NSE blocks bare requests** → prime cookies via `nseindia.com` (Route A) / `niftyindices.com` (Route B1) first, then send browser headers.
2. **Index `Volume` = 0** — indices aren't traded; don't treat as a data error.
3. **Date tokens differ:** Route A file = `DDMMYYYY`; niftyindices payload = `DD-Mmm-YYYY`; NSE api = `DD-MM-YYYY`. Easy to mix up.
4. **Weekend/holiday = 404** on Route A → skip; keep an NSE holiday list.
5. **Don't hardcode the index list** — derive from `Index Name` in the daily file so new/renamed indices flow through automatically.
6. **Be polite:** one session, `sleep(~0.4s)`, cache to disk — NSE/niftyindices throttle/ban hammering.
7. **EOD vs intraday:** these are daily index closes. For intraday index candles use the Groww `get_historical_candles` API on the index groww_symbol (e.g. `NSE-NIFTY`) — separate layer.
8. **niftyindices response shape can change** — it returns a JSON string inside `d`; parse defensively and print the first row once to confirm field names before mapping.

---

## VERIFICATION STATUS (this session)

| Item | Status |
| --- | --- |
| `ind_close_all_<DDMMYYYY>.csv` path | ⚠️ verify-first (long-stable NSE path) |
| `ind_close_all` columns / one-file-all-indices | ✅ confirmed via real-file usage |
| niftyindices `getHistoricaldatatabletoString` POST | ✅ verified endpoint + payload |
| NSE `api/historical/indicesHistory` | ⚠️ verify-first |
| Yahoo symbols `^NSEI`, `^CRSLDX`, `^BSESN` | ✅ verified |
| Yahoo `^NSEBANK`, `^INDIAVIX` | ⚠️ smoke-test symbol once |
| ~421 NSE indices total (Jan 2026) | ✅ per NSE/Anand Rathi |
| Index taxonomy lists | ✅ core set (not full 421 enumeration) |

**First thing to run:** `list_live_indices(date(2026,6,12))` → prints the actual N index names NSE published that day. That single call answers "how many / which indices" definitively, straight from the source.
