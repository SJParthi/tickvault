# EMF `metric_selectors` COST NOTE — 2026-08-09 (metric-blindness fix)

> **Relocated 2026-08-10, verbatim.** This note previously lived as a comment
> block inside `deploy/aws/terraform/user-data.sh.tftpl`. EC2 caps `user_data`
> at 16,384 bytes and the template was measured at **16,382** — two bytes of
> headroom — so the terraform plan failed the moment anything was added. The
> note is documentation, not executable content, so it moved here rather than
> being deleted. The selector literal it describes is unchanged and still lives
> in the template (and in `deploy/aws/cloudwatch-agent.json`).
> Size ratchet: `crates/common/tests/user_data_size_guard.rs`.

The selector in the template is an EXPLICIT, EXACT-NAME alternation — never a
prefix wildcard. That is deliberate: CloudWatch bills ~$0.30/custom-metric/month,
the live budget kill-ceiling is $100/mo, and its AUTOMATIC budget actions fire
`STOP_EC2_INSTANCES` at 90% ($90). A `tv_.*`-style prefix would publish whatever
the binary happens to emit and could stop the trading box mid-session, so the
count must stay auditable by reading one line.

```
before 2026-08-09 : 11 names ≈ $3.30/mo
after  2026-08-09 : 41 names ≈ $12.30/mo   (delta +30 names ≈ +$9.00/mo)
```

The workspace emits ~352 distinct metric names; 311 are deliberately NOT
selected. Inclusion rule applied: a name is selected only if it means FAILURE,
SATURATION, or DATA LOSS *and* has a reachable producer on the REST-only
runtime.

## Deliberately EXCLUDED

- **Success/volume counters + latency histograms** (`tv_*_fetch_total`,
  `tv_*_close_to_data_ms`, …) — they answer "how much", not "what broke"; the
  `/metrics` endpoint + `errors.jsonl` keep them for post-hoc triage.
- **Names whose ONLY emit sites are the stood-down per-minute boot legs**
  (`[spot_1m_rest]` / `[option_chain_1m]` / `[groww_*_1m]` `enabled=false` since
  2026-07-17 — the cadence executors own the pulls), e.g.
  `tv_spot1m_sid_not_served_total`, `tv_*_sweep_still_missing_total`.
- **Names behind the non-default `groww_orders` cargo feature** (Gate 2 of the
  §39.2 lattice — not compiled into the deploy build), e.g.
  `tv_groww_push_supervisor_respawn_total`.
- ~~**`tv_ws_frame_spill_write_errors_total`** — no WS frame producer exists (both
  live feeds retired 2026-07-13/15)~~ **STALE, CORRECTED 2026-08-12 — now
  SELECTED.** The Dhan live WS lane was revived 2026-08-09/11 and IS a WS frame
  producer, so the stated reason stopped holding the day the lane came back and
  the exclusion silently survived it. This is the exact failure the closing
  sentence of this section warns about, running in reverse: a stale EXCLUSION
  hides a live producer's loss signal just as effectively as a dead INCLUSION
  advertises coverage that cannot exist. It matters more than most, because
  `ws_frame_spill::accept` returns `Spilled` even when the disk is full (the
  writer thread drains and discards), so this counter is the ONLY signal that
  the capture-at-receipt durable floor has stopped holding. The boot-time replay
  counter `tv_wal_replay_corrupted_segments_total` remains selected because
  `replay_all` runs every boot.
- **Tick-persist loss counters — SELECTED 2026-08-12** (`tv_ticks_dropped_total`,
  `tv_tick_persist_errors_total`, `tv_tick_rows_refused_total`). Same cause: the
  ingest-side loss counters (`tv_dhan_ws_wal_dropped_total`,
  `tv_dhan_feed_seals_dropped_total`, …) were added when the lane was revived,
  but the PERSIST-side ones were not — so the pipeline was instrumented at the
  socket and blind at the database, while `dhan_enabled = true` and
  `TICKVAULT_DHAN_LIVE_FEED=1`. `tv_ticks_dropped_total` in particular fires on
  the flush-failure path that DISCARDS the buffered rows, which is the single
  largest tick-loss window in the live lane.
- **`tv_api_auth_failed_total`** — already published via its own log metric
  filter (`auth-failed-alarm.tf`); EMF-selecting it would double-bill.

A dead name in this list implies coverage no producer can publish, which is the
false-OK class this repo's guards forbid (audit-findings Rule 11).

## LOCKSTEP

The selector literal MUST stay byte-identical between
`deploy/aws/terraform/user-data.sh.tftpl` and `deploy/aws/cloudwatch-agent.json`
— pinned by `crates/storage/tests/cw_agent_selector_lockstep_guard.rs`. The
exact count is pinned by `crates/common/tests/cloudwatch_app_alarms_wiring.rs`.
