# No REST Except Live Feed — Market-Data REST Lock (Operator Lock 2026-06-27) — SUMMARY STUB

> **Full text:** `docs/claude-rules-full/project/no-rest-except-live-feed-2026-06-27.md` (moved verbatim 2026-09-26 to keep the auto-loaded context small). **Read the full file before adding, removing or changing ANY broker REST call, token/auth REST, instrument-master download, cross-verification, S3 archival gate, or a table/alarm/metric tied to a REST leg.** It is a dated log — later sections supersede earlier ones (§8 → §9 → §10 → §11 → §12 → §12.10 → §12.15 and its sub-sections). Where this summary and the full file differ, the full file wins. Amend the full file first, then keep this summary in sync.

**Original quote (2026-06-27):** "As of now except live feed any rest urls should never ever be used anywhere in dhan or groww dude okay?"

**§2 (standing boundary):** a literal "kill ALL REST" is self-contradictory; the allowed classes are **live-feed AUTH** and **static instrument-master CSVs** — without them the sockets cannot connect or map a tick.

**Current state (§12, 2026-09-16 SOCKETS-ONLY, as narrowed by §12.10 and partially reversed by §12.15):**
- Operator: "Dude except sockets remove all the entire remaining rest api call related implementations ddue okay?", narrowed by "Bro just remove per minute price falls and 3.41 pm accuracy check alone dude okay".
- **KEEP:** AUTH REST (`generateAccessToken`, `RenewToken`, the minter Lambda); INSTRUMENT IDENTITY REST (Dhan detailed master CSV, niftyindices constituents); non-broker HTTP (QuestDB, Telegram, AWS SDK). **ORDER SIDE untouched.**
- **REMOVED:** per-minute spot-1m pull, per-minute option-chain pull, expirylist.
- **§12.15 (2026-09-24) RESTORED — verification only:** `POST /v2/charts/intraday`, interval `"1"`, ONCE per trading day after 15:41 IST, for the main-feed spot/index targets (plus the §12.15.6 depth-held option pass). Never written to `ticks` / `candles_*`. The daily S3 archive of day D is **held until a `dhan_live_crossverify` day marker exists for D** (marker only on a MEASURED verdict; hold ceiling `MAX_CROSSVERIFY_HOLD_DAYS`; disk-pressure leg NOT gated). The live lane never waits for the verifier.

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
