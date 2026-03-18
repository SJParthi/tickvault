# Implementation Plan: Expand Index Constituency to All NSE Indices

**Status:** VERIFIED
**Date:** 2026-03-18
**Approved by:** Parthiban

## Summary

Expand `INDEX_CONSTITUENCY_SLUGS` from 6 indices to 49 NSE indices covering all broad-based, sectoral, and thematic categories.

## Plan Items

- [x] 1. Expand `INDEX_CONSTITUENCY_SLUGS` from 6 to 49 indices
  - Files: `crates/common/src/constants.rs`
  - Categories: 16 broad-based + 15 sectoral + 18 thematic/strategy

- [x] 2. Update `INDEX_CONSTITUENCY_MIN_INDICES` from 3 to 15
  - Files: `crates/common/src/constants.rs`

- [x] 3. Update `IndexConstituencyConfig` defaults: `inter_batch_delay_ms` 0→200
  - Files: `crates/common/src/config.rs`

- [x] 4. Add slug validation tests: no duplicates, no empty names, URL-safe chars
  - Files: `crates/core/tests/index_constituency.rs`
  - Tests: `test_all_slugs_valid_format`, `test_no_duplicate_slugs`, `test_no_duplicate_index_names`, `test_slug_count_covers_all_categories`

- [x] 5. Build & test pass (2439+ tests, fmt, clippy)
  - Files: N/A
