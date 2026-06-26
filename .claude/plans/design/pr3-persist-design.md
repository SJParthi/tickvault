# PR-3 Design — Persist the per-feed toggle across restarts

## Goal
The operator's last webpage feed-toggle choice (Dhan/Groww enable/disable) must
STICK across an app restart, WITHOUT rewriting the git-tracked locked config
`config/base.toml`. Achieved via a separate `data/feed-state.json` overlay:
- Atomic write on every successful `POST /api/feeds/{feed}` toggle.
- Boot reads `FeedsConfig` from config as the DEFAULT, then OVERLAYS the
  persisted file (if present + valid) to compute the EFFECTIVE per-feed
  enabled state used to seed `FeedRuntimeState` and gate lane spawns.

## Where things live today (verified)
- `crates/api/src/feed_state.rs` — `FeedRuntimeState::from_config(&FeedsConfig)`
  seeds atomics; `set_enabled(feed, bool)`; `snapshot() -> FeedStatus`.
- `crates/api/src/handlers/feeds.rs::set_feed` — the toggle endpoint; calls
  `state.feed_runtime().set_enabled(feed, req.enabled)` after the safety gate.
  Already behind GAP-SEC-01 bearer auth (`protected_routes` in lib.rs).
- `crates/app/src/main.rs:239-245` — `let feeds = &config.feeds;` then
  `FeedRuntimeState::from_config(feeds)`. Lane spawn gates read
  `feeds.dhan_enabled` (`:267`, `:355`, `:1143`) and `feeds.groww_enabled`.
- Atomic-write precedent: `instrument_snapshot.rs::write_plan_snapshot`
  (tmp `.json.tmp` → `fs::rename`) + `summary_writer.rs::atomic_write`
  (tmp + `sync_all().with_context(...)?` → rename). Path-traversal guard
  precedent: `instrument_snapshot::is_valid_trading_date` + parent-dir recheck.
- Data-dir precedent: `CACHE_BASE_DIR = "data/instrument-cache"`.

## Module: `crates/api/src/feed_state_persist.rs` (NEW)
Pure, std-only (no new dep — api crate already has serde/serde_json/chrono).

### Constants
- `FEED_STATE_DIR: &str = "data"`              — the pinned data directory.
- `FEED_STATE_FILENAME: &str = "feed-state.json"`.
- `feed_state_path() -> PathBuf` = `PathBuf::from(FEED_STATE_DIR).join(FEED_STATE_FILENAME)`.

### JSON schema (`PersistedFeedState`, serde, camel-free snake_case)
```json
{ "dhan_enabled": false, "groww_enabled": true, "updated_at_ist": "2026-06-26 14:31:07" }
```
- `dhan_enabled: bool`, `groww_enabled: bool` — the persisted per-feed desire.
- `updated_at_ist: String` — IST wall-clock stamp of the write
  (`chrono::Utc::now() + IST offset`, `"%Y-%m-%d %H:%M:%S"`), human-readable.
  `#[serde(default)]` so an older file without it still parses.

### Path validation (defence-in-depth, mirrors instrument_snapshot)
`validate_feed_state_path(path) -> bool`:
1. file name MUST equal `FEED_STATE_FILENAME` (no traversal via the name).
2. parent MUST equal exactly `Path::new(FEED_STATE_DIR)`.
   → both constants are compile-time fixed (no operator/runtime input feeds the
   path), so traversal is structurally impossible; the test pins the invariant
   and guards a future refactor that might parameterise the path.

### `persist_feed_state(status: &FeedStatus, path: &Path) -> io::Result<()>`
1. validate the path (reject → `InvalidInput` error, caller logs, no crash).
2. `create_dir_all(parent)`.
3. serialise `PersistedFeedState { dhan_enabled, groww_enabled, updated_at_ist }`
   with `serde_json::to_vec_pretty`.
4. write `{path}.tmp` → `File::sync_all().with_context`-style (here: `?` on
   each io op) → `fs::rename(tmp, path)` (atomic on same fs).
   Mirrors `instrument_snapshot::write_plan_snapshot` + summary_writer sync.

### `load_feed_state(path: &Path) -> Option<PersistedFeedState>`
- file missing (`ErrorKind::NotFound`) → `None` (NOT an error — use config default).
- read or JSON-parse error → `None` (corrupt/partial file → fall back to config),
  logged WARN by the boot caller (a partial `.tmp` is never renamed in, so a
  truncated `feed-state.json` only happens on a torn rename — rare; we tolerate
  it by failing to `None`).
- Ok → `Some(state)`.

### `overlay_feeds(config: FeedsConfig, persisted: Option<PersistedFeedState>) -> FeedsConfig`
Pure function (unit-testable, no I/O):
- `None` → return config unchanged (config default wins).
- `Some(p)` → `FeedsConfig { dhan_enabled: p.dhan_enabled, groww_enabled: p.groww_enabled }`.
Returns the EFFECTIVE config the boot uses for BOTH `FeedRuntimeState::from_config`
AND the `feeds.dhan_enabled` / `feeds.groww_enabled` lane-spawn gates.

## Wiring

### Toggle handler (`feeds.rs::set_feed`)
After the successful `state.feed_runtime().set_enabled(feed, req.enabled)` (and
after the existing info/warn honesty logs), call:
```rust
let snap = state.feed_runtime().snapshot();
if let Err(err) = feed_state_persist::persist_feed_state(
        &snap, &feed_state_persist::feed_state_path()) {
    tracing::error!(?err, feed = feed.as_str(),
        "failed to persist feed-state overlay (data/feed-state.json) — the \
         runtime toggle is LIVE this session but will NOT survive a restart \
         until the write succeeds");
}
```
- Persist from the *full snapshot* (both flags) so the file always reflects the
  complete desired state, not just the one flag toggled (one feed's file write
  carries the other feed's current value too — no lost update).
- Failure is `error!` (Rule 5: persist failures route to Telegram) but does NOT
  fail the HTTP response — the live toggle already succeeded; persistence is the
  durable add-on. Honest envelope in the message.

### Boot (`main.rs` ~line 239)
Replace `let feeds = &config.feeds;` region with:
```rust
let persisted = tickvault_api::feed_state_persist::load_feed_state(
    &tickvault_api::feed_state_persist::feed_state_path());
if persisted.is_some() {
    info!(... "feed-state overlay found — restoring the last webpage toggle choice");
}
let effective_feeds = tickvault_api::feed_state_persist::overlay_feeds(
    config.feeds.clone(), persisted);
let feeds = &effective_feeds;   // everything below already reads `feeds.*`
```
- `FeedRuntimeState::from_config(feeds)` now seeds from the EFFECTIVE state.
- The lane-spawn gates (`if feeds.dhan_enabled`, `if !feeds.dhan_enabled`,
  `feeds.groww_enabled`) all already read `feeds`, so they pick up the overlay
  with ZERO further edits.
- `config.feeds.dhan_enabled` direct reads at `:1143-1144` and the source-scan
  guard at `:7176` reference `config.feeds.*` — those are the FALLBACK/source
  string; check whether `:1143` is a real gate that must also use `effective`.
  (Will read that site during impl; if it is a live gate, switch it to a local
  effective copy too.)

## HONEST BOUNDARY (no illusion — must be in operator-facing text)
- Persisting a feed ON → it boots ON (the inline boot honours the effective
  flag; the lane is spawned at boot as today).
- Persisting Dhan/Groww OFF → desired-OFF at next boot (lane not spawned),
  correct.
- The DEFERRED residual (PR-2 honest boundary) is unchanged: a feed persisted
  OFF and then toggled ON *at runtime* still cannot cold-start the full inline
  Dhan boot spine from cold — that needs the deferred spine-lift. Persistence
  changes the *next-boot* effective state, which IS the durable path: persist
  ON → restart → boots ON. So "the choice sticks" is fully honest for the
  restart path (which is exactly what the operator asked for).

## Failure modes
| Mode | Behaviour |
|---|---|
| `data/feed-state.json` missing | `load_feed_state` → `None` → config default. No error. |
| File corrupt / invalid JSON | `load_feed_state` → `None` (parse err swallowed→None) → config default; boot logs WARN. Never crashes. |
| Unwritable `data/` dir on toggle | `persist_feed_state` → `Err`; handler logs `error!`, HTTP toggle still 200 (live). Next successful write self-heals. |
| Partial write (crash mid-write) | tmp file orphaned; real file untouched (rename is atomic) → next boot reads last good file or config. |
| Torn rename leaving truncated file | `load_feed_state` parse fails → `None` → config default (fail-safe). |
| Path traversal attempt | structurally impossible (fixed constants); `validate_feed_state_path` rejects + test pins it. |

## Tests (named)
In `feed_state_persist.rs` `#[cfg(test)]`:
- `test_persist_feed_state_writes_atomically_and_round_trips` — write then load
  returns the same flags (atomic write + load roundtrip).
- `test_feed_state_overlay_atomic_write` — assert no `.tmp` left after a write;
  file content parses to the written flags.
- `test_boot_overlay_read_overrides_config` — `overlay_feeds(config_dhan_on_groww_off,
  Some(groww_on_dhan_off))` returns dhan_off/groww_on (persisted wins).
- `test_overlay_none_keeps_config_default` — `overlay_feeds(cfg, None) == cfg`.
- `test_corrupt_feed_state_falls_back_to_config` — write garbage bytes to the
  file → `load_feed_state` returns `None` → `overlay_feeds(cfg, None) == cfg`.
- `test_missing_feed_state_file_is_none_not_error` — load on nonexistent path → None.
- `test_validate_feed_state_path_rejects_traversal` — wrong filename / wrong
  parent → false; the canonical `data/feed-state.json` → true.
- `test_persisted_feed_state_serde_round_trip` + `test_persisted_state_without_updated_at_parses`
  (serde default).

Each test uses a unique temp dir under `std::env::temp_dir()` (like
summary_writer tests) — never touches the real `data/` dir.

## Guarantees recap
(a) toggle persists atomically — tmp+rename, sync, error-logged on failure.
(b) boot overlay-read overrides config — `overlay_feeds` + `let feeds = &effective`.
(c) missing/corrupt → config fallback, no crash — `load_feed_state` → None paths.
(d) path validation prevents traversal — fixed constants + `validate_feed_state_path`.
(e) GAP-SEC-01 intact — persistence is added INSIDE `set_feed`, behind the
    unchanged bearer-auth `protected_routes`; no new public route.
