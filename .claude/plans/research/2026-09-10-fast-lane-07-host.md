# Fast-lane 07 — HOST: fast consumer at 12,500 pps? (2026-09-10)

**V**=Verified (file:line), **A**=Assumed, **U**=Unknown. Extends `2026-09-09-31-agent-audit-and-r8gd-decision.md` §3.

## 1. Host knobs

| Knob | Current | Fast-lane implication | Recommended | Cost/risk |
|---|---|---|---|---|
| `AllowedCPUs` | `1-2` (V `tickvault.service:256`); QuestDB `cpuset 2,3` (V `docker-compose.yml:187`); IRQs → cpu 0 (V `apply-host-tuning.sh:92,124`) | 16 readers + drain + 2 tokio workers **and** six OS writer threads (V `dhan_feed_stack.rs:1947,2028,6628,6666,11648`, `ws_frame_spill.rs:914`, `main.rs:3351`) share two cores, one with QuestDB. A writer descheduling a tokio worker is the host-side lag path (app 6.7% of box 2026-08-18, V `main.rs:355-357`, pre-depth). | Keep; measure `top -H` at 09:15 | Adding core 3 starves QuestDB WAL apply |
| tokio | 2 workers (V `:263`, `main.rs:468-471`); no `event_interval` (V grep) | Fine; drain is one task | None | — |
| `MemoryHigh`/`OOMScoreAdjust` | `20G` throttle, no `MemoryMax`; `-900` vs QDB `-500` (V `:323,354`, compose `:196`) | Throttle slows the drain — 371,828 throttle events 2026-09-03 (V `:137`); reached only on replay bursts | Keep; fix replay | `WatchdogSec=150` (V `:155`) |
| `LimitNOFILE`/`StartLimitBurst` | 65536 / 5 per 3600 s (V `:266,75-76`) | None | Keep | — |
| Receive buffers | `rmem_max` 128 MiB, `tcp_rmem 4096 1M 128M`, autotune (V `99-tickvault-net.conf:75-78`; installed `tickvault-host-tuning.service:58`) | Sized for 75k pps (V `:19-22`); 12.5k×162 B ≈ 2 MB/s; no app `SO_RCVBUF` (V `:63-64`) | Keep | — |
| `netdev_max_backlog`/`budget` | 65536 / 1200 @ 8000 µs (V `:89,102-103`) | ~0.87 s NIC→stack buffer | Keep | — |
| RPS/XPS, busy_poll | RPS off (V `apply-host-tuning.sh:139-142`); busy_poll absent (V grep); irqbalance stopped (V `:103`) | All ENA vectors on core 0. **A** fine at 12.5k; **U** >50k | Keep; busy-poll would eat the app core | — |
| Writeback | `dirty_background 3` / `dirty 10` (V `:166-167`) ≈ 1/3.2 GiB | Bounds the disk-stall→socket-fill path (V `:145-161`). **Doubles on 64 GiB** | On any 64 GiB move use `*_bytes` | — |
| THP/hugepages/cpufreq/BBR | THP `madvise` (V `apply-host-tuning.sh:30-32`); BBR probed (V `user-data.sh.tftpl:224-226`); no hugepages/governor (V grep) | Neutral | None | — |
| QuestDB | `mem_limit 12g`, `shm 512m`, WAL/shared workers 2/2, uncommitted 2M, O3 lag 60 s, append page 16 MiB (V compose `:127,199,326,342,257,259,273`) | WAL apply lag (2 workers vs ~1.5 G depth rows/session), not the feed, is the measured bottleneck | Unchanged on 4 vCPU | — |

**Verdict:** kernel side holds the open burst with ~6× margin. The host does **not** guarantee "zero lag": the drain shares two cores with six writer threads and QuestDB, with no thread priority and a throttle that can deschedule it. The recorded p99 46 s lag is vendor-side; no host knob touches it.

## 2. r8g.xlarge vs r8gd.2xlarge (this workload)

| | r8g.xlarge (locked: V `variables.tf:33`, `daily-universe…md:1466`) | r8gd.2xlarge |
|---|---|---|
| CPU partition | 0 IRQ · 1 app · 2 app+QDB · 3 QDB | 0 IRQ · 1–2 tokio · 3–4 writers · 5–7 QDB (3 WAL workers) — **A** |
| Headroom (16 readers + drain + writers + QDB) | shared, unisolated | writers off the drain cores; QDB +1 apply worker |
| Hot partitions | 600 GB gp3 6000/500 (V `main.tf:394-397`), 19%/21% used | NVMe, unmetered, **wiped on stop** (V `daily-universe…md:200,451,1249`; Quote 13 rejected r8gd) |
| Data at 17:30 | all on EBS | today's partition is never archivable (V `partition_archive.rs:114,398`, active exclusion `:23`) ⇒ **≥1 session lost per wipe** unless QuestDB stays on EBS — which forfeits the NVMe |
| Knobs to re-derive | — | `MemoryHigh`, `QDB_MEM_LIMIT`, `dirty_*`, rmem budget ratchet |
| Price/month | $147.76 incl GST; forecast $142.24 vs **$135** line (V `daily-universe…md:863-864`) | $103.83 data-on-NVMe; **≈$183** if EBS kept for QuestDB (**A**) — over $135 and $150 |
| Edits | none | §7 Rule 1: seven lockstep sites + dated quote |

## 3. S3 — plain answer

1. Partitions older than the floor are exported as `csv.gz` to `s3://…/questdb-partitions/<table>/<partition>.csv.gz` (V `partition_archive.rs:49,926`), verified count+length+SHA-256 (V `:53-60`), then audit-gated `DROP PARTITION` (V `:61-67`) — preserved.
2. Today's depth/ticks and today's+yesterday's candles are **never eligible** (V `:110-118,370-398`) — the code cannot "archive everything before each stop", so an NVMe wipe loses them.
3. **Yes — once dropped, gone from QuestDB.** S3 holds CSV, not a partition; **no ATTACH/import path exists** (V grep: zero). Cold = not queryable in the live DB until a re-ingest tool is built.
