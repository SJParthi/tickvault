# Broker API Docs — Raw Reference Import (Operator Extraction 2026-07-15)

> **Source:** operator-provided extraction via Claude Cowork, captured **2026-07-15**
> from the live public documentation sites of four Indian brokers / market-data
> vendors. Uploaded to this repo on 2026-07-16.
>
> **Status: RAW REFERENCE material — NOT authoritative.**
> `docs/dhan-ref/` + the `.claude/rules/` enforcement files remain the
> authoritative enforcement source for all Dhan facts (and `docs/groww-ref/`
> for Groww) until a dedicated reconciliation pass reviews and merges any
> deltas from this import. Do NOT cite files in this folder as ground truth
> for code changes; do NOT modify or delete the authoritative trees in the
> name of this import.

## Contents (all plain text: markdown + a few raw html/txt crawler backups)

| Folder | Provider | Files | Covers |
|---|---|---|---|
| `dhan/` | DhanHQ API v2 (dhanhq.co/docs/v2) | 26 | Auth, orders, super/forever orders, conditional trigger, portfolio, EDIS, trader's control, funds, statements, postback, live order update WS, live market feed WS (binary byte-offset tables), 20/200-level depth, market quote REST, option chain, historical + expired options data, annexure (enums, DH-901…910 + 800–814), scrip master CSV schema, releases v2→v2.5, `dhanhq` Python SDK v2.2.0 |
| `groww/` | Groww Trade API (groww.in/trade-api/docs) | 19 | Auth (token API, TOTP + checksum flows), instruments CSV schema, orders, smart orders (GTT/OCO), portfolio, margin, live data, websocket feed, historical + backtesting, annexures, rate limits, GA000–GA007 errors, `growwapi` Python SDK v1.5.0, changelog |
| `truedata/` | TrueData (truedata.in) | 15 | WS auth + port map, realtime WS tick/bar/greeks messages, history REST, analytics/option chain/greeks, symbol masters, Python/Node.js/.NET SDKs, Velocity COM API, limits/errors/FAQ |
| `globaldatafeeds/` | GlobalDataFeeds / GFDL (globaldatafeeds.in) | 74 | WS + REST market data APIs, streaming, option chain, Greeks, delayed, gainers/losers, code samples, pricing, FAQ, the full Fundamental Data API (`28*`) + News API (`29*`), widgets, release notes, `ws-gfdl` library. `_raw/` + `_decoded/` are crawler scratch backups (raw HTML) |
| `UPSTREAM-README.md` | — | 1 | The extraction's own README: capture method, verification (4 crawler agents + independent audit), known gaps |

Each provider folder carries `00-INDEX.md` (file map) and `SOURCES.md`
(every URL fetched, with ok/failed/gated status).

## Provenance & integrity notes

- Content is a verbatim capture of the public doc sites as of 2026-07-15,
  including the sites' own typos (flagged inline by the extractor).
- Secret-scanned before import: no tokens, keys, or operator identifiers.
  The only key-like strings are the doc sites' own published examples
  (a truncated JWT-header sample in the Groww auth page; GFDL's public
  "Try It" demo key in their OpenAPI `example:` fields).
- Known upstream gaps (gated/not publicly available) are recorded — never
  fabricated — in `UPSTREAM-README.md` and the per-provider `SOURCES.md`.

## Synthesized data catalogs (added 2026-07-16 — NOT raw vendor docs)

- `catalog-truedata.md` and `catalog-gfdl.md` are **SYNTHESIZED** catalogs,
  compiled 2026-07-16: 10-category field-level data catalogs with verbatim
  citations into the `truedata/` and `globaldatafeeds/` raw docs in this
  folder plus cited web sources. They are reconciled indexes INTO the raw
  capture — they are NOT raw vendor docs and carry the same
  non-authoritative status as the rest of this folder.
- Both catalogs conclude the vendor's push WebSocket is a **~1-second
  conflated L1 snapshot stream, not tick-by-tick** — consistent with the
  `docs/gdf-ref/README.md` reconciled claims table.

## Follow-up (separate task, NOT this import)

A reconciliation pass should diff this capture against `docs/dhan-ref/` /
`docs/groww-ref/` and the `.claude/rules/dhan/` enforcement files, and merge
any real deltas through the normal rule-file update protocol (dated operator
quotes where locks apply).
