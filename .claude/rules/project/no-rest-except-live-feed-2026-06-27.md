# No REST Except Live Feed — Market-Data REST Lock (Operator Lock 2026-06-27) — SUMMARY STUB

> **Full text:** `docs/claude-rules-full/project/no-rest-except-live-feed-2026-06-27.md` (moved verbatim 2026-09-26 to keep the auto-loaded context small). **Read the full file before adding, removing or changing ANY broker REST call, token/auth REST, instrument-master download, cross-verification, S3 archival gate, or a table/alarm/metric tied to a REST leg.** It is a dated log — later sections supersede earlier ones (§8 → §9 → §10 → §11 → §12 → §12.10 → §12.15 and its sub-sections). Where this summary and the full file differ, the full file wins. Amend the full file first, then keep this summary in sync.

**Original quote (2026-06-27):** "As of now except live feed any rest urls should never ever be used anywhere in dhan or groww dude okay?"

**§2 (standing boundary):** a literal "kill ALL REST" is self-contradictory; the allowed classes are **live-feed AUTH** and **static instrument-master CSVs** — without them the sockets cannot connect or map a tick.

**Current state (§12, 2026-09-16 SOCKETS-ONLY, as narrowed by §12.10 and partially reversed by §12.15):**
- Operator: "Dude except sockets remove all the entire remaining rest api call related implementations ddue okay?", narrowed by "Bro just remove per minute price falls and 3.41 pm accuracy check alone dude okay".
- **KEEP:** AUTH REST (`generateAccessToken`, `RenewToken`, the minter Lambda); INSTRUMENT IDENTITY REST (Dhan detailed master CSV, niftyindices constituents); non-broker HTTP (QuestDB, Telegram, AWS SDK). **ORDER SIDE untouched.**
- **REMOVED:** per-minute spot-1m pull, per-minute option-chain pull, expirylist.
- **§12.15 (2026-09-24) RESTORED — verification only:** `POST /v2/charts/intraday`, interval `"1"`, ONCE per trading day after 15:41 IST, for the main-feed spot/index targets (plus the §12.15.6 depth-held option pass). Never written to `ticks` / `candles_*`. The daily S3 archive of day D is **held until a `dhan_live_crossverify` day marker exists for D** (marker only on a MEASURED verdict; hold ceiling `MAX_CROSSVERIFY_HOLD_DAYS`; disk-pressure leg NOT gated). The live lane never waits for the verifier.
- **§12.15.7 (2026-10-06) the day marker means what it says:** the marker counts only after tmp + fsync + rename and a strict content read; "persisted" means zero audit rows discarded or refused; a failed marker write is a failed attempt, retried marker-only and paged only after the last attempt on the existing `xverify_failed` source; cross-verification markers are kept 400 days. REJECT: a unit-returning marker write, an `is_file()`-only reader, a sweep shorter than the gated hold lookback, counting a discarded row as persisted.
- **§12.15.8 (2026-10-06) every attempt ends by 17:23 under a timeout:** the end bound is the 17:25 scheduled-stop window minus 120 s (17:23), retries are 760 s apart, a late attempt runs with a shrunk budget or is skipped below 120 s, every full attempt runs under `tokio::time::timeout`, the synchronous audit persist after its last `.await` stops itself at the same instant (it never starts a flush whose worst case, 5 s + bytes at 100 KiB/s, ends after it), the last attempt is decided once, before it starts, a day's only attempt skipped for time is reported at once and retried once at 17:45 if the process is still up, and the day is paged once across processes (a same-day paged marker; a skipped day pages `xverify_failed` `skipped_no_time` unless that marker exists). A deliberate persist stop at the deadline never counts on a §2.10 loss-group counter. A marker-only attempt is one file write under no timeout. REJECT: an attempt that can end after 17:23, a persist flush started past the deadline, lengthening `attempt_max_secs`, deciding the last attempt from its end time, a skipped day that neither pages nor finds the paged marker, a second page while the paged marker exists, a deadline stop counted on a loss-group counter.

**REJECT (headlines from §12.6 / §12.10.6 / §12.15.4):**
- Removes the AUTH REST calls or the token minter, or the daily master CSV / niftyindices constituent fetch.
- Removes a leg's HTTP while leaving its ALARM, EMF metric name, or metric filter in place; removes a WRITER and leaves a READER on the frozen table.
- Deletes `spot_1m_rest` / `option_chain_1m` / `rest_fetch_audit` TABLE ROWS, any SEBI/audit row, or DROPs a retained table.
- Retires the liveness alarm, or re-points it while changing `period` or `evaluation_periods`; deletes `SPOT_1M_REST_INDICES`.
- Touches the order-side REST surface, `dry_run`, or the §28 frozen indicator/strategy area under cover of this directive.
- Weakens or removes the WAL floor or any other refusal floor.
- Re-adds any per-minute REST pull; makes the live lane wait for the verifier; writes a marker on a vacuous or failed run; gates the disk-pressure archive leg; holds past `MAX_CROSSVERIFY_HOLD_DAYS` without a loud coded error; writes the vendor tape into `ticks` or `candles_*`.
- Claims the feed is verified beyond what the check actually compared.

"Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this rule file FIRST with a dated quote."
