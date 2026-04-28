# Wave 4 Worst-Case Catalog — Categories C + D + E

> **Authority:** companion to `.claude/plans/active-plan-wave-4.md`
> **Scope:** Wave-4-E2 sub-PR enumerates every scenario below into a
> chaos test + ErrorCode + ratchet.

## Category C — Storage failures (6 scenarios)

| # | Scenario | Detection | Recovery primitive | ErrorCode | Chaos test |
|---|---|---|---|---|---|
| C1 | Disk full during spill write | `statvfs` < 1GB free at spill enqueue | Pre-flight check (PR #406 item 9); abort write before fsync; emit CRITICAL | STORAGE-GAP-05 | `chaos_disk_full_during_spill.rs` |
| C2 | QuestDB partition corruption | ILP write returns 5xx with "partition" in error | Detach corrupted partition, retain data in spill, emit CRITICAL | (covered by AUDIT-NN catch-all) | `chaos_questdb_partition_corruption.rs` |
| C3 | QuestDB ILP TCP port unreachable | Connect refused / timeout | Existing 3-tier resilience: rescue ring → spill → DLQ | (existing) | covered by chaos_questdb_docker_pause |
| C4 | Valkey kill mid-session | Connection refused on next op | Existing token cache fallback to SSM (Scenario 4 in disaster-recovery) | (existing) | covered by chaos_valkey_kill |
| C5 | Spill file gets too large (filling boot disk) | `tv_spill_file_size_bytes > threshold` | Telegram WARN at 50% of free space; HALT at 90% | RESOURCE-03 (E3) | `chaos_spill_file_growth.rs` |
| C6 | QuestDB clock skew vs host clock | `select now()` vs local time delta > 2s | BOOT-03 already covers boot-time check; add runtime probe every 5min | BOOT-03 (existing Wave-2-C) | extend existing test |

## Category D — Auth failures (4 scenarios)

| # | Scenario | Detection | Recovery primitive | ErrorCode | Chaos test |
|---|---|---|---|---|---|
| D1 | TOTP secret rotated externally (without SSM update) | `generateAccessToken` fails with INVALID_TOTP on first attempt despite our secret unchanged | Telegram CRITICAL; HALT until operator updates SSM | AUTH-GAP-04 | `chaos_token_external_rotation.rs` |
| D2 | JWT expires mid-WebSocket session | DataAPI-807 disconnect code | Existing AUTH-GAP-02 token refresh + reconnect | (existing) | covered by existing |
| D3 | Wake-from-sleep with stale token (>4h validity left required) | `next_renewal_at()` < 4h | AUTH-GAP-03 force_renewal_if_stale | (existing) | covered by existing |
| D4 | API Key & Secret consent flow returns invalid token | First API call returns DH-901 | Operator action only; Telegram CRITICAL with link to consent flow runbook | (existing DH-901) | manual runbook |

## Category E — Dhan API behavior surprises (6 scenarios)

| # | Scenario | Detection | Recovery primitive | ErrorCode | Chaos test |
|---|---|---|---|---|---|
| E1 | Dhan accepts subscribe but never streams (silent black-hole) | No packets in 60s post-subscribe for instruments that are otherwise live | Resubscribe once after 60s; if still silent, mark instrument unreachable + Telegram | DH-911 | `chaos_dhan_subscribe_blackhole.rs` |
| E2 | Dhan returns partial response (truncated TCP frame) | Parser detects `actual_size != header.message_length` | Discard partial; log + counter; do NOT panic | (existing parser error log) | `chaos_dhan_partial_response.rs` |
| E3 | Dhan rate-limit error 805 (too many connections) | DataAPI-805 disconnect | STOP_ALL 60s pause per `annexure-enums` rule 12 | (existing DATA-805) | covered by existing |
| E4 | Dhan returns DH-904 storm (rate limit) on order API | 5+ DH-904 in 60s on order endpoint | Exponential backoff 10/20/40/80s; circuit breaker opens | (existing DH-904) | covered by existing |
| E5 | Instrument master CSV column rename (Dhan releases v3 schema) | CSV parser fails to find `SEM_INSTRUMENT_NAME` | HALT + Telegram CRITICAL with link to `instrument-master.md` rule | (existing I-P0-* family) | `chaos_csv_column_rename.rs` |
| E6 | Dhan introduces new ExchangeSegment value (e.g. enum 6 newly populated) | Binary header parse `from_byte()` returns `None` for known-valid packet | Log + counter increment; do NOT panic; skip the packet | (existing) | `chaos_unknown_exchange_segment.rs` |

## Trigger

This file activates when editing:
- `crates/storage/src/tick_persistence.rs` (spill pre-flight)
- `crates/core/src/auth/*.rs` (D1–D4)
- `crates/core/src/parser/*.rs` (E2, E5, E6)
- Any chaos test matching C1–C6 / D1–D4 / E1–E6
