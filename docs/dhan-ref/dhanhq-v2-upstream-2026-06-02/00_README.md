# DhanHQ v2 — Split Documentation (one file per source page)

Audited 2 June 2026. API v2. Python client `dhanhq` 2.2.0.
Each file below is a faithful, standalone markdown of one source page so Claude Code can load only what it needs.
For the consolidated single-file version (with cross-cutting gotchas), see `../DhanHQ_v2_API_Reference.md`.

| File | Source URL |
|---|---|
| `01_introduction.md` | https://dhanhq.co/docs/v2/ |
| `02_authentication.md` | https://dhanhq.co/docs/v2/authentication/ |
| `03_live_market_feed.md` | https://dhanhq.co/docs/v2/live-market-feed/ |
| `04_full_market_depth.md` | https://dhanhq.co/docs/v2/full-market-depth/ |
| `05_historical_data.md` | https://dhanhq.co/docs/v2/historical-data/ |
| `06_expired_options_data.md` | https://dhanhq.co/docs/v2/expired-options-data/ |
| `07_option_chain.md` | https://dhanhq.co/docs/v2/option-chain/ |
| `08_annexure.md` | https://dhanhq.co/docs/v2/annexure/ |
| `09_instruments.md` | https://dhanhq.co/docs/v2/instruments/ |
| `10_releases.md` | https://dhanhq.co/docs/v2/releases/ |
| `11_live_order_update.md` | https://dhanhq.co/docs/v2/order-update/ |
| `12_chart_optionchart.md` | https://api.dhan.co/v2/#/operations/optionchart  → `POST /charts/rollingoption` |
| `13_chart_intradaycharts.md` | https://api.dhan.co/v2/#/operations/intradaycharts → `POST /charts/intraday` |
| `14_chart_historicalcharts.md` | https://api.dhan.co/v2/#/operations/historicalcharts → `POST /charts/historical` |
| `15_python_sdk_dhanhq.md` | https://pypi.org/project/dhanhq/ + https://github.com/dhan-oss/DhanHQ-py |
| `16_postback.md` | https://dhanhq.co/docs/v2/postback/ |
| `17_statements.md` | https://dhanhq.co/docs/v2/statements/ |
| `18_funds.md` | https://dhanhq.co/docs/v2/funds/ |
| `19_traders_control.md` | https://dhanhq.co/docs/v2/traders-control/ |
| `20_edis.md` | https://dhanhq.co/docs/v2/edis/ |
| `21_portfolio.md` | https://dhanhq.co/docs/v2/portfolio/ |
| `22_market_quote.md` | https://dhanhq.co/docs/v2/market-quote/ |
| `23_conditional_trigger.md` | https://dhanhq.co/docs/v2/conditional-trigger/ |

**Grouping** (by API family, for quick orientation)
- **Core / setup:** `01` intro, `02` auth, `08` annexure (enums), `09` instruments, `10` releases, `15` Python SDK.
- **Trading APIs:** `16` postback, `17` statements, `18` funds & margin, `19` trader's control, `20` eDIS, `21` portfolio & positions, `23` conditional trigger, `11` live order update. *(Orders / Super Order / Forever Order pages were not in the requested set — their endpoints/signatures are in `15_python_sdk_dhanhq.md` and the appendix of `../DhanHQ_v2_API_Reference.md`.)*
- **Data APIs:** `22` market quote, `03` live market feed, `04` full market depth, `05` historical data, `06` expired options, `07` option chain, plus the chart-operation schema files `12`/`13`/`14`.

**Notes**
- Files 12–14 are extracted from the live OpenAPI spec (`https://api.dhan.co/v2/v3/api-docs`); they are the machine-schema view of the same endpoints documented narratively in `05` (intraday/daily) and `06` (rolling/expired). Where the spec and the docs-site disagree (notably the timestamp epoch), the discrepancy is noted in each file.
- File 15 merges PyPI (install/usage) and GitHub (source structure) since both describe the one Python client.
- All WebSocket payloads are binary, little-endian. Two different header byte-orders exist (feed vs depth) — see `03` and `04`.
