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

---

## 2026-08-21 — the user-data byte budget is now the binding constraint on observability

EC2 user-data has a hard **16,384-byte** limit and terraform refuses the PLAN
above it, failing every unrelated resource in the same run. The selector list
is duplicated inline in `user-data.sh.tftpl`, so every metric name is spent
twice: once in `deploy/aws/cloudwatch-agent.json`, once in the template that
must fit the limit.

The margin is now small enough that **adding a metric means removing one.**

That happened on 2026-08-21, and it is recorded because the trade was real:

| Metric | Outcome | Why |
|---|---|---|
| `tv_depth_rows_dropped_total` | **shipped** | depth row LOSS; emitted for months, shipped by nothing, while the rows-WRITTEN counter beside it was shipped — depth read healthy off-box while its losses were unobservable |
| `tv_depth_persist_errors_total` | **shipped** | same blindness; its `HOT-PATH-02` coded line has no errcode alarm either, so there was no second path |
| `tv_ilp_rows_discarded_total` | **shipped** | the new ILP retention bound; a bound that discards invisibly is worse than the leak it replaced |
| `tv_order_fill_lag_seconds` | **removed** | the only entry in the whole allowlist with ZERO emit sites in `crates/*/src` — a name nothing could ever publish |
| `tv_risk_mark_rejected_total` | **NOT shipped** | ~28 bytes short |
| `tv_dhan_feed_silence_detector_refused` | **NOT shipped** | ~37 bytes short |

The last two are emitted by production code and are visible on the box's own
`:9091/metrics` and in coded log lines. They do **not** reach CloudWatch, so
no alarm can read them and no dashboard can chart them. That is a real gap and
it is stated here rather than left to be discovered from a quiet panel.

This is a scaling wall, not a tight fit, and it fails in the worst direction:
the thing it silently rations is the ability to SEE failures.

### The fix, and why it was deliberately not taken in that change

The EMF/prometheus half of the agent config is *already* a repo file
(`deploy/aws/cloudwatch-agent.json`), and the lockstep guard already pins the
two selector literals byte-identical. The structural move is to stop inlining
it: keep only the host-metrics and log-collection base in user-data, and after
the Step 5 repo clone apply the prometheus half with the agent's documented
multi-config mechanism —

```
/opt/aws/amazon-cloudwatch-agent/bin/amazon-cloudwatch-agent-ctl \
    -a append-config -m ec2 -s -c file:/opt/tickvault/repo/deploy/aws/cloudwatch-agent.json
```

— after substituting the environment into its log-group names (the repo file
hardcodes `prod`; the template uses `$${ENVIRONMENT}`). Then the selector can
grow to any size, and a failed clone degrades to host metrics and logs instead
of taking the whole terraform plan down.

It was not done in the same change because it moves the boot path and cannot
be tested anywhere but a real EC2 first boot. If `append-config` were wrong,
**every app metric would vanish silently** — precisely the failure the change
existed to prevent. It wants its own change, with a boot verified on the box.

Until then, every new metric costs an existing one. Make that trade
explicitly; do not discover it at 16,385 bytes.
