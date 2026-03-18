# Implementation Plan: Expand Index Constituency to All NSE Indices

**Status:** DRAFT
**Date:** 2026-03-18
**Approved by:** pending

## Summary

Expand `INDEX_CONSTITUENCY_SLUGS` from 6 indices to ~50 NSE indices covering all broad-based, sectoral, and thematic categories. Update thresholds and config defaults accordingly. Keep existing architecture (separate lookup via `IndexConstituencyMap`).

## Plan Items

- [ ] 1. Expand `INDEX_CONSTITUENCY_SLUGS` from 6 to ~50 indices
  - Files: `crates/common/src/constants.rs`
  - Categories: 16 broad-based + 15 sectoral + ~18 thematic/strategy
  - Tests: existing `slug_count_matches_constant` auto-validates via `.len()`

- [ ] 2. Update `INDEX_CONSTITUENCY_MIN_INDICES` from 3 to 15
  - Files: `crates/common/src/constants.rs`
  - Tests: compile-time constant, no separate test needed

- [ ] 3. Update `IndexConstituencyConfig` defaults: `max_concurrent_downloads` 3→5, `inter_batch_delay_ms` 0→200
  - Files: `crates/common/src/config.rs`
  - Tests: existing default tests

- [ ] 4. Add slug validation tests: no duplicates, no empty names, URL-safe chars, all slugs non-empty
  - Files: `crates/core/tests/index_constituency.rs`
  - Tests: `test_all_slugs_valid_format`, `test_no_duplicate_slugs`, `test_no_duplicate_index_names`

- [ ] 5. Build & test pass
  - Files: N/A
  - Tests: `cargo test --workspace`

## Complete Index List (~50 indices)

### Broad-Based (16)
| Display Name | CSV Slug |
|---|---|
| Nifty 50 | `ind_nifty50list` |
| Nifty Next 50 | `ind_niftynext50list` |
| Nifty 100 | `ind_nifty100list` |
| Nifty 200 | `ind_nifty200list` |
| Nifty 500 | `ind_nifty500list` |
| Nifty Midcap 50 | `ind_niftymidcap50list` |
| Nifty Midcap 100 | `ind_niftymidcap100list` |
| Nifty Midcap 150 | `ind_niftymidcap150list` |
| Nifty Midcap Select | `ind_niftymidcap_selectlist` |
| Nifty Smallcap 50 | `ind_niftysmallcap50list` |
| Nifty Smallcap 100 | `ind_niftysmallcap100list` |
| Nifty Smallcap 250 | `ind_niftysmallcap250list` |
| Nifty LargeMidcap 250 | `ind_niftylargemidcap250list` |
| Nifty MidSmallcap 400 | `ind_niftymidsmallcap400list` |
| Nifty Microcap 250 | `ind_niftymicrocap250_list` |
| Nifty Total Market | `ind_niftytotalmarket_list` |

### Sectoral (15)
| Display Name | CSV Slug |
|---|---|
| Nifty Auto | `ind_niftyautolist` |
| Nifty Bank | `ind_niftybanklist` |
| Nifty Financial Services | `ind_niftyfinancelist` |
| Nifty Financial Services 25/50 | `ind_niftyfinancialservices25_50list` |
| Nifty FMCG | `ind_niftyfmcglist` |
| Nifty Healthcare | `ind_niftyhaborealthcarelist` |
| Nifty IT | `ind_niftyitlist` |
| Nifty Media | `ind_niftymedialist` |
| Nifty Metal | `ind_niftymetallist` |
| Nifty Pharma | `ind_niftypharmalist` |
| Nifty PSU Bank | `ind_niftypsubanklist` |
| Nifty Private Bank | `ind_niftypvtbanklist` |
| Nifty Realty | `ind_niftyrealtylist` |
| Nifty Consumer Durables | `ind_niftyconsumerdurablelist` |
| Nifty Oil & Gas | `ind_niftyoilandgaslist` |

### Thematic / Strategy (18)
| Display Name | CSV Slug |
|---|---|
| Nifty Commodities | `ind_niftycommoditieslist` |
| Nifty India Consumption | `ind_niftyindiaconsumptionlist` |
| Nifty CPSE | `ind_niftycpselist` |
| Nifty Energy | `ind_niftyenergylist` |
| Nifty Infrastructure | `ind_niftyinfrastructurelist` |
| Nifty MNC | `ind_niftymnclist` |
| Nifty PSE | `ind_niftypselist` |
| Nifty Growth Sectors 15 | `ind_niftyGrowthSectors15list` |
| Nifty100 Quality 30 | `ind_nifty100_Quality30list` |
| Nifty50 Value 20 | `ind_nifty50_value20list` |
| Nifty Alpha 50 | `ind_niftyAlpha50list` |
| Nifty High Beta 50 | `ind_niftyHighBeta50list` |
| Nifty Low Volatility 50 | `ind_niftyLowVolatility50list` |
| Nifty India Digital | `ind_niftyIndiaDigital_list` |
| Nifty India Defence | `ind_niftyIndiaDefence_list` |
| Nifty India Manufacturing | `ind_niftyIndiaMfg_list` |
| Nifty Mobility | `ind_niftymobility_list` |
| Nifty500 Multicap 50:25:25 | `ind_nifty500Multicap502525_list` |

## Note on Slug Reliability

niftyindices.com slug naming is inconsistent (some `list`, some `_list`, casing varies). The existing architecture handles this gracefully:
- Individual download failures are logged and skipped
- Partial results are always returned
- Cache fallback on network failure
- `INDEX_CONSTITUENCY_MIN_INDICES = 15` means at least 15 must succeed

If a slug is wrong, that index silently fails and the rest work fine. We can fix individual slugs after seeing download logs.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | All ~50 CSVs download OK | Map with ~50 indices, 500+ unique stocks |
| 2 | Some slugs return 404 | Individual failures logged, map built with available |
| 3 | niftyindices.com down | All fail → cache fallback → None if no cache |
| 4 | Stock in multiple indices | Reverse map lists all indices for that stock |

## Principles Check

| Principle | Compliance |
|-----------|-----------|
| Zero allocation on hot path | YES — constituency download is cold path (boot only) |
| O(1) or fail at compile time | YES — HashMap lookups are O(1) |
| Every version pinned | YES — no new dependencies |
