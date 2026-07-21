# Program state at EAP shutdown (2026-07-21)

## Merged last 48h (all via All Green)

#1685 sign-flip, #1692 capacity record, #1691 order-leg P&L, #1684 rules diet 2, #1689 cadence hardening, #1681+#1695 dep bumps, #1694 coverage floors, #1693 native retry ladder (selective per-target T+2/3/3.8s spot rungs, chain single T+4 slot, cross-fill arbitration, CADENCE-05, T+30/T+50 history re-pull, keep-alive fix), #1698 cross-fill digest observability. #1690+#1680 closed superseded.

## OPEN: PR #1696 (branch claude/tf-diet-11-broker-1d) — TF reshape

CONTRACT IN FLUX: yesterday's approved plan = 11 frames (1-5m,15m,1h-4h,1d broker-pulled next-morning; 30m removed). TODAY the operator issued a NEW frame directive (verbatim, typos included): "1 seocnd till 15 seocnd sequantially one by oen and 30 second and 1m, 3m, 5m, 15m alone" — i.e. 1s..15s + 30s + 1m/3m/5m/15m; replace-vs-add and the 1d question were posed to the operator, UNANSWERED at shutdown. A WIP preservation commit (TF_COUNT 21→12, 6m-14m retired, incl. the partition_retention_coverage_guard.rs bare-21-literal fix) may exist on the branch. Note: 1s-scale frames need the GDF per-second feed as source and are a deeper rebuild (TfIndex bucket math, seal/spill format, DDL are minute-scale).

## NEW APPROVED DIRECTION (operator, 2026-07-21, quotes in coordinator transcript)

GDF SubscribeRealtime trial — NIFTY 50 spot + current-expiry full chain 49CE+49PE = 99 symbols, every-second L1; greeks computed by BruteX's own engine; slippage playbook: spread-gate entries, limit-at-ask, no-chase rule, per-second slippage audit. Blocked on: GDF trial key/endpoint (ask operator), gdf plan file flip to APPROVED (operator quotes exist). The gdf-third-feed-scope-2026-07-13.md lock governs; docs/gdf-ref/ is the reference; the honest data envelope: ~1/sec conflated L1, update-driven (55-58% second-coverage on liquid futs, indices every second), NO depth, NO sub-second, history has NO 1s bars (TICK≈7d, 1m≈3mo). Nobody retail sells true TBT (verdict + broker table delivered 2026-07-21; TrueData #1 engineering, GDF #2 seam-ready, Zerodha #3).

## PENDING OPERATOR DECISIONS

1. Frame-set clarification above.
2. Capacity: 08:30 IST t4g.medium start failed 2 days running, 08:45 watchdog saved both — options (a) keep watchdog [default] (b) 08:15 cron (~+₹30/mo, draft PR exists) (c) type/AZ change.
3. Paper order-push flip (both brokers' channels built, default OFF; day-0 checklist drafted).
4. Whether the 15:47 IST 2026-07-21 cross-fill digest Telegram reached the operator's phone (app-side dispatch PROVEN — if not received, suspect chat-id SSM value).

## AUDIT VERDICTS

GROWW order stack 0 critical, live-fire lattice VERIFIED-LOCKED, 4 HIGHs = unbuilt READ endpoints, unwired readiness probe, unexercised push, unprobed entitlement. DHAN audit partial (3/7 modules): 1 HIGH DhanOrderResponse all-#[serde(default)] absent-vs-zero ambiguity (types.rs:485-535), MED no request timeouts, MED reactive-only 429; fix was assigned but its session died pre-code — NOT FIXED.

## OPERATIONAL FACTS

Prod = t4g.medium, 08:30-16:30 IST auto, binary deploys post-close; 2026-07-21 session was cleanest ever (Dhan 375/375, zero open misses); Groww NIFTY chain dies expiry Tuesdays ~14:51 IST (2 weeks running, cross-fill covers it); SPOT-XVERIFY mismatches trending 81→91.
