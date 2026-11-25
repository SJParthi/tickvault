# brutex Strategy Host — tickvault runs only approved brutex strategies (Operator Lock 2026-10-29)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` > this file > `brutex-readonly-lock-2026-07-18.md` > `daily-universe-scope-expansion-2026-05-27.md` §28 > defaults.
> **Scope:** PERMANENT. Every session, every PR, every machine that builds tickvault.
> **Contract:** `docs/architecture/brutex-strategy-contract.md` (the crate API, the recipe, manifest, check-file and snapshot formats).
> **Auto-load:** always (path is in `.claude/rules/project/`).

## §0. The owner's decisions, 29 Oct 2026 (verbatim headings, final)

> "MY DECISIONS (29 Oct 2026). They are final; record them in this repo's rules and docs so every future session follows them."
> 1. "Only strategies I explicitly approve for live trading ever reach tickvault."
> 2. "tickvault runs BRUTEX'S OWN strategy code, never a re-implementation. brutex will keep all strategy logic in one standalone Rust crate. ... tickvault depends on that crate as a Cargo git dependency pinned to the exact brutex commit that produced the approved backtest (rev = <sha>). A newly approved version moves the pin in a reviewed tickvault release. tickvault can act ONLY when brutex's code is accessible to it: every machine that builds tickvault (my Mac, CI, AWS) must be able to READ github.com/SJParthi/brutex. Use read-only access only (a deploy key or a fine-grained read-only token, kept in the machine's git credential store or AWS SSM, NEVER in this repo)."
> 3. "brutex publishes each approved strategy to s3://tv-prod-cold/strategy-recipes/<strategy-name>/v<N>/ (v1, v2 ... NEVER overwritten)" with recipe.json, three check files and manifest.json.
> 4. "Dynamic strategies ... publish three layers: the approved METHOD (v<N>) ...; PARAMETER snapshots under v<N>/params/<date>/; UNIVERSE snapshots ... Refreshes are automatic, but each must pass the gates (Winner Gate on the newest walk-forward window, sanity limits against the previous snapshot, and tickvault's replay match). On any failure the previous snapshot stays live and I get an alert."
> 5. "tickvault builds candles from its own live ticks. Before using any version or snapshot, it replays the check candles through the pinned brutex crate and compares indicator values and signals bar by bar; any difference refuses it. It switches only before the market opens, never mid-session. Paper trading comes before real money."
>
> Background, same message: "tickvault has NO strategy logic of its own. Its existing modules crates/trading/src/indicator and crates/trading/src/strategy hold none of my strategies, and they must not be used to trade brutex strategies."

## §1. The rule (one line)

**tickvault trades only strategies the owner approved, by running brutex's own strategy crate at the exact approved commit, after the published check files replay bit for bit through it; it defines no strategy of its own and never writes to brutex or to the recipe bucket.**

## §2. Mechanical contract

| Aspect | Locked value |
|---|---|
| Strategy logic | brutex's standalone Rust crate only, via Cargo `git` + `rev = "<40-char sha>"`; never a re-implementation in tickvault |
| Old modules | `crates/trading/src/indicator` and `crates/trading/src/strategy` are never used to trade a brutex strategy (fencing proposal pending the owner; nothing is removed without asking) |
| Pin moves | only in a reviewed tickvault release; nothing moves it automatically |
| Build access | every build machine (Mac, CI, AWS) reads `github.com/SJParthi/brutex` with a READ-ONLY deploy key or fine-grained read-only token from the machine's git credential store or AWS SSM; never committed; no build machine gets write access |
| Recipe bucket | `s3://tv-prod-cold/strategy-recipes/` is READ-ONLY to tickvault (List + Get only); tickvault never puts, copies over, or deletes there |
| Selection | newest `v<N>` with `status: approved`; recipe `version` must equal the folder; recipe `brutex_commit` must equal the commit tickvault is built against |
| Verification | manifest sha256 + size for every file; a missing, extra, resized or changed file refuses the folder |
| Snapshots | each must pass `gates.json` (Winner Gate), sanity limits against the previous snapshot, and the replay match; on failure the previous snapshot stays live and the owner gets ONE alert |
| Replay | check candles → pinned crate → values and signals compared bar by bar, bit for bit; the first difference (file, row, bar time, instrument, field, expected, got) refuses it |
| Swap | only before 09:00 IST on a trading day; never mid-session |
| Audit | every load, refusal and swap is written to the audit log |
| Money | paper trading before real money; this file authorizes no live order |
| Hot path | the crate is deterministic, no I/O, no blocking, no allocation per tick after `configure`, O(1) per tick at p99 (contract §2.1 R1–R10) |

## §3. Rules this decision amends (recorded here, rule-file-first)

- `brutex-readonly-lock-2026-07-18.md` §1.1 "Code hygiene: no code vendored into tickvault": amended for ONE thing only — the approved strategy crate as a pinned Cargo git dependency (a read). Every write path to bruteX stays forbidden.
- `groww-second-feed-scope-2026-06-19.md` §4 REJECT "Pulls, vendors, copies, or imports ANY code from the brutex repo": same single exception.
- `deny.toml` `unknown-git = "deny"`: when the crate exists, `allow-git = ["https://github.com/SJParthi/brutex"]` is added in the same PR as the dependency, nothing wider.
- `daily-universe-scope-expansion-2026-05-27.md` §28.5 (the same-day speed-only lift for the old indicator/strategy modules, if merged): WITHDRAWN — those modules do not trade brutex strategies, so speeding them up serves no live path.
- `dhan-rest-only-noise-lock-2026-07-14.md`: decision 4's alert ("I get an alert") is the dated quote for ONE new page family, "strategy version or snapshot refused", edge-triggered once per refused version/snapshot per day; its row in that file lands with the code that sends it.

## §4. REJECT (any PR that does one of these)

- Adds strategy logic of tickvault's own, or trades a brutex strategy through `crates/trading/src/indicator` or `crates/trading/src/strategy`.
- Depends on brutex by branch, tag only, version range, path, or vendored copy instead of `rev = "<sha>"`.
- Moves the pin automatically, or without the matching approved recipe version.
- Commits a key, token or credential, or gives any build machine write access to brutex.
- Writes, copies over or deletes anything under `s3://tv-prod-cold/strategy-recipes/`.
- Uses a version or snapshot without manifest verification, gates and a passing replay, or swaps mid-session.
- Places a real-money order from a brutex strategy before paper trading and a fresh dated owner quote.
- Silences a refusal (no alert, no audit row).

"Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this rule file FIRST with a dated quote."

## §5. Auto-driver explanation

> Sir, brutex is the test kitchen where recipes are invented and tasted for years of past data. tickvault is the restaurant. The restaurant never invents a dish: it cooks only recipes you signed, with the test kitchen's own tools, and before serving it cooks the tasting plate again and checks every spoon matches. If one spoon differs, that recipe is not served and yesterday's stays on the menu, and you get one message.
