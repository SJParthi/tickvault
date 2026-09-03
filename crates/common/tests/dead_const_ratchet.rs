//! Shrink-only ratchet on DEAD `pub const`s in `crates/common/src/constants.rs`.
//!
//! ## The class this closes
//!
//! A named constant reads to every future reader as if it enforces something.
//! Nobody re-greps it. So it can sit in production for months protecting
//! nothing, while its own doc comment describes in detail the failure it is
//! failing to prevent. Three instances were found and fixed on 2026-08-15:
//!
//!   - `MAX_PLAUSIBLE_LTP` — declared since Phase 0, ZERO references, while its
//!     doc described the candle corruption it was not stopping. `high` is a
//!     running `max`, so one `f32::MAX` packet pinned a bucket at 3.4e38. WIRED.
//!   - `DH901_ROTATE_MAX_RETRIES` — zero references; the annexure rule it named
//!     was enforced elsewhere by a bool. RETIRED (wiring it would have made a
//!     regulatory "never more than once" tunable).
//!   - `MINIMUM_VALID_EXCHANGE_TIMESTAMP` — zero references, and DUPLICATED a
//!     live two-sided bound with a looser value. RETIRED.
//!
//! A workspace audit then found **167 of 521** `pub const`s in that file with no
//! production reference at all. This test freezes that number as a ceiling.
//!
//! ## Why a test and not a hook
//!
//! The repo's other ratchet guards (test-count, pub-fn, financial) keep their
//! baselines in GITIGNORED files, so CI cannot run them — a CI run would
//! establish a fresh baseline and pass vacuously, which
//! `merge-gate-lock-2026-07-04.md` §3 row 6 records explicitly. The allowlist
//! below is COMMITTED source, so this runs in the normal Test (common) lane and
//! cannot be vacuous.
//!
//! ## Why the STALE-ENTRY half matters more than the dead-const half
//!
//! An allowlist that only ever grows is not a ratchet, it is a wishlist. This
//! test fails in BOTH directions:
//!
//!   - a NEW dead const not on the list → fail (the ceiling holds);
//!   - a listed const that is now WIRED or DELETED → fail, demanding its removal
//!     from the list (the ceiling comes down and can never drift back up).
//!
//! Without the second half, wiring a constant would leave a permanent free slot
//! for the next dead one, and the count would look like progress while standing
//! still.
//!
//! ## Honest limits
//!
//! This is a TEXT scan, not a compiler query, and it is deliberately generous
//! about what counts as "alive": any word-boundary occurrence of the name in any
//! `crates/*/src/**.rs` outside `constants.rs`. A mention inside a comment
//! therefore counts as a reference, so this UNDER-reports dead constants. That
//! direction is chosen on purpose — a guard whose first act is a batch of false
//! positives teaches its readers to weaken it.
//!
//! It also covers only `constants.rs`. Constants declared in other modules are
//! out of scope; extending it is a follow-up, not a silent gap.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// `pub const`s in `constants.rs` with ZERO production references, frozen as of
/// 2026-08-15. **This list may only SHRINK.**
///
/// Adding an entry means shipping a new constant that enforces nothing, and
/// needs a reviewer to say why that is better than not declaring it. Removing
/// one means the constant was wired or retired — the outcome this exists to
/// drive.
const DEAD_CONST_ALLOWLIST: &[&str] = &[
    "APPLICATION_NAME",
    "APPLICATION_VERSION",
    "APP_DAILY_RESET_TIME_IST",
    "BAR_FINAL_SEAL_OFFSET_SECS",
    "BINARY_CACHE_FILENAME",
    "BOOT_STEP_AUTH_TIMEOUT_SECS",
    "BOOT_STEP_IP_WHITELIST_TIMEOUT_SECS",
    "BOOT_STEP_QUESTDB_DDL_TIMEOUT_SECS",
    "CANDLES_PER_TRADING_DAY",
    "CANDLE_FLUSH_BATCH_SIZE",
    "CAS_WINDOW_CLOSE_SECS_OF_DAY_IST",
    "CAS_WINDOW_OPEN_SECS_OF_DAY_IST",
    "CSV_COLUMN_DISPLAY_NAME",
    "CSV_COLUMN_EXCH_ID",
    "CSV_COLUMN_EXPIRY_DATE",
    "CSV_COLUMN_EXPIRY_FLAG",
    "CSV_COLUMN_INSTRUMENT",
    "CSV_COLUMN_LOT_SIZE",
    "CSV_COLUMN_OPTION_TYPE",
    "CSV_COLUMN_SECURITY_ID",
    "CSV_COLUMN_SEGMENT",
    "CSV_COLUMN_SERIES",
    "CSV_COLUMN_STRIKE_PRICE",
    "CSV_COLUMN_SYMBOL_NAME",
    "CSV_COLUMN_TICK_SIZE",
    "CSV_COLUMN_UNDERLYING_SECURITY_ID",
    "CSV_COLUMN_UNDERLYING_SYMBOL",
    "CSV_EXCHANGE_BSE",
    "CSV_EXCHANGE_NSE",
    "CSV_INSTRUMENT_FUTIDX",
    "CSV_INSTRUMENT_FUTSTK",
    "CSV_INSTRUMENT_OPTIDX",
    "CSV_INSTRUMENT_OPTSTK",
    "CSV_OPTION_TYPE_CALL",
    "CSV_OPTION_TYPE_PUT",
    "CSV_SEGMENT_DERIVATIVE",
    "CSV_SEGMENT_EQUITY",
    "CSV_SEGMENT_INDEX",
    "CSV_SERIES_EQUITY",
    "CSV_TEST_SYMBOL_MARKER",
    "DEDUP_RING_BUFFER_POWER",
    "DEPTH_FLUSH_BATCH_SIZE",
    "DHAN_CANDLE_INTERVAL_15MIN",
    "DHAN_CANDLE_INTERVAL_1MIN",
    "DHAN_CANDLE_INTERVAL_25MIN",
    "DHAN_CANDLE_INTERVAL_5MIN",
    "DHAN_CANDLE_INTERVAL_60MIN",
    "DHAN_CHARTS_HISTORICAL_PATH",
    "DHAN_ERROR_CODE_MAX_LEN",
    "DHAN_NSE_HOLIDAY_CROSS_CHECK_URL",
    "DISCONNECT_ACCESS_TOKEN_EXPIRED",
    "DISCONNECT_ACCESS_TOKEN_INVALID",
    "DISCONNECT_AUTHENTICATION_FAILED",
    "DISCONNECT_CLIENT_ID_INVALID",
    "DISCONNECT_DATA_API_SUBSCRIPTION_REQUIRED",
    "DISCONNECT_EXCEEDED_ACTIVE_CONNECTIONS",
    "DISPLAY_INDEX_COUNT",
    "DISPLAY_INDEX_ENTRIES",
    "DIVIDEND_YIELD",
    "DOCKER_CONTAINER_PREFIX",
    "FEED_REQUEST_CONNECT",
    "FRAME_CHANNEL_CAPACITY",
    "FULL_CHAIN_INDEX_COUNT",
    "FULL_CHAIN_INDEX_SYMBOLS",
    "GRACEFUL_SHUTDOWN_TIMEOUT_SECS",
    "ILP_FLUSH_BATCH_SIZE",
    "INDEX_CONSTITUENCY_CSV_CACHE_FILENAME",
    "INDEX_CONSTITUENCY_KNOWN_STALE_SLUGS",
    "INDEX_CONSTITUENCY_MIN_INDICES",
    "INDEX_CONSTITUENCY_RETRY_MAX_DELAY_SECS",
    "INDEX_CONSTITUENCY_RETRY_MAX_TIMES",
    "INDEX_CONSTITUENCY_RETRY_MIN_DELAY_SECS",
    "INDEX_CONSTITUENCY_SLUG_COUNT",
    "INSTRUMENT_CSV_DOWNLOAD_TIMEOUT_SECS",
    "INSTRUMENT_CSV_MAX_DOWNLOAD_RETRIES",
    "INSTRUMENT_CSV_MIN_BYTES",
    "INSTRUMENT_CSV_MIN_ROWS",
    "INSTRUMENT_CSV_RETRY_INITIAL_DELAY_MS",
    "INSTRUMENT_CSV_RETRY_MAX_DELAY_MS",
    "INSTRUMENT_FRESHNESS_MARKER_FILENAME",
    "INTRADAY_TIMEFRAMES",
    "IV_MAX_BOUND",
    "IV_MIN_BOUND",
    "IV_SOLVER_MAX_ITERATIONS",
    "IV_SOLVER_TOLERANCE",
    "MARKET_CLOSE_TIME_IST_EXCLUSIVE",
    "MARKET_DEPTH_OFFSET_DEPTH_START",
    "MARKET_DEPTH_OFFSET_LTP",
    "MARKET_DEPTH_PACKET_SIZE",
    "MARKET_LAST_CANDLE_START_IST",
    "MARKET_OPEN_TIME_IST",
    "MARKET_STATUS_CLOSED",
    "MARKET_STATUS_OPEN",
    "MARKET_STATUS_POST_CLOSE",
    "MARKET_STATUS_PRE_OPEN",
    "MAX_STRATEGY_INSTANCES",
    "MAX_TOTAL_SUBSCRIPTIONS",
    "MAX_TOTAL_SUBSCRIPTIONS_TARGET",
    "MAX_WEBSOCKET_FRAME_SIZE",
    "MIN_DAILY_UNIVERSE_SIZE",
    "MUHURAT_PERSIST_END_SECS_OF_DAY_IST",
    "MUHURAT_PERSIST_START_SECS_OF_DAY_IST",
    "NTM_CONSTITUENCY_FETCH_TIMEOUT_SECS",
    "NTM_CONSTITUENCY_SLUGS",
    "ORDER_EVENT_RING_BUFFER_CAPACITY",
    "ORDER_UPDATE_BROADCAST_CAPACITY",
    "PERIODIC_HEALTH_CHECK_INTERVAL_SECS",
    "PHASE_0_IDX_I_COUNT",
    "PHASE_0_IDX_I_SYMBOLS",
    "POST_MARKET_FETCH_WINDOW_END_SECS_OF_DAY_IST",
    "POST_MARKET_FETCH_WINDOW_START_SECS_OF_DAY_IST",
    "QUESTDB_IST_ALIGN_OFFSET",
    "QUESTDB_TABLE_BUILD_METADATA",
    "QUESTDB_TABLE_CANDLES_1S",
    "QUESTDB_TABLE_DERIVATIVE_CONTRACTS",
    "QUESTDB_TABLE_FNO_UNDERLYINGS",
    "QUESTDB_TABLE_INDEX_CONSTITUENTS",
    "QUESTDB_TABLE_NSE_HOLIDAYS",
    "QUESTDB_TABLE_PREVIOUS_CLOSE",
    "QUESTDB_TABLE_SUBSCRIBED_INDICES",
    "RISK_FREE_RATE",
    "SEBI_AUDIT_RETENTION_YEARS",
    "SELF_TRADE_COOLDOWN_SECS",
    "SERVER_PING_INTERVAL_SECS",
    "STATIC_IP_BOOT_RETRY_INTERVAL_SECS",
    "STATIC_IP_BOOT_RETRY_MAX_ATTEMPTS",
    "STOCK_ATM_STRIKES_ABOVE",
    "STOCK_ATM_STRIKES_BELOW",
    "STOCK_CONTRACTS_PER_EXPIRY",
    "TICK_FLUSH_BATCH_SIZE",
    "TICK_FLUSH_INTERVAL_MS",
    "TICK_RING_BUFFER_CAPACITY",
    "TICK_SPILL_MIN_DISK_SPACE_BYTES",
    "TIMEFRAME_15M",
    "TIMEFRAME_1D",
    "TIMEFRAME_1M",
    "TIMEFRAME_5M",
    "TIMEFRAME_60M",
    "TOKEN_RENEWAL_CIRCUIT_BREAKER_THRESHOLD",
    "TOTAL_SUBSCRIBED_INDEX_COUNT",
    "TWENTY_DEPTH_BODY_SIZE",
    "TWO_HUNDRED_DEPTH_BODY_SIZE",
    "VALIDATION_FNO_STOCK_MAX_COUNT",
    "VALIDATION_FNO_STOCK_MIN_COUNT",
    "VALIDATION_MIN_DERIVATIVE_COUNT",
    "VALIDATION_MUST_EXIST_EQUITIES",
    "VALIDATION_MUST_EXIST_FNO_STOCKS",
    "VALIDATION_MUST_EXIST_INDICES",
    "WEBSOCKET_AUTH_TYPE",
    "WEBSOCKET_PROTOCOL_VERSION",
    "WS_GRACE_AFTER_CLOSE_SECS",
    "WS_GRACE_AFTER_CLOSE_SECS_U32",
    "WS_RATE_LIMIT_BACKOFF_BASE_MS",
    "WS_RATE_LIMIT_BACKOFF_CAP_MS",
    "WS_SHORT_SESSION_RECONNECT_FLOOR_MS",
    "WS_SHORT_SESSION_THRESHOLD_MS",
];

fn workspace_root() -> PathBuf {
    // crates/common -> crates -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root must resolve")
}

fn constants_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/constants.rs")
}

/// Every `pub const NAME` declared in `constants.rs`.
fn declared_consts(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("pub const ") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        if !name.is_empty() {
            out.insert(name);
        }
    }
    out
}

/// Concatenated text of every production `crates/*/src/**.rs` EXCEPT
/// `constants.rs` itself — read once, then searched 500+ times.
fn production_sources_except_constants() -> String {
    let root = workspace_root();
    let skip = constants_path()
        .canonicalize()
        .expect("constants.rs must exist");
    let mut buf = String::new();
    let mut stack = vec![root.join("crates")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Production source only: `tests/`, `benches/`, `examples/`
                // are NOT call sites for this purpose — a constant referenced
                // only by a test that asserts it equals its own literal is
                // exactly the tautology class this guard exists to surface.
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if matches!(name, "tests" | "benches" | "examples" | "target") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // Only files under a `src/` directory count as production.
            if !path.components().any(|c| c.as_os_str() == "src") {
                continue;
            }
            if path.canonicalize().map(|p| p == skip).unwrap_or(false) {
                continue;
            }
            if let Ok(text) = fs::read_to_string(&path) {
                buf.push_str(&text);
                buf.push('\n');
            }
        }
    }
    assert!(
        buf.len() > 100_000,
        "production source scan collected only {} bytes — the walk is broken and every \
         constant would look dead, which would make this guard fire on everything (or, \
         with an inverted check, pass on everything)",
        buf.len()
    );
    buf
}

/// Word-boundary containment: `MAX_X` must not match inside `MAX_X_TOTAL`.
fn references(haystack: &str, name: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = haystack[from..].find(name) {
        let start = from + rel;
        let end = start + name.len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

const fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[test]
fn no_new_pub_const_may_be_declared_without_a_call_site() {
    let text = fs::read_to_string(constants_path()).expect("read constants.rs");
    let declared = declared_consts(&text);
    assert!(
        declared.len() > 400,
        "only {} pub consts parsed from constants.rs — the parser broke and this guard \
         would pass vacuously",
        declared.len()
    );

    let sources = production_sources_except_constants();
    let allow: BTreeSet<&str> = DEAD_CONST_ALLOWLIST.iter().copied().collect();

    let mut new_dead: Vec<&str> = Vec::new();
    for name in &declared {
        if !references(&sources, name) && !allow.contains(name.as_str()) {
            new_dead.push(name);
        }
    }

    assert!(
        new_dead.is_empty(),
        "these `pub const`s in constants.rs have ZERO production references — they enforce \
         NOTHING while reading as if they do, the exact class that let MAX_PLAUSIBLE_LTP sit \
         un-wired since Phase 0 while its own doc described the corruption it was failing to \
         stop.\n\nFor each: WIRE it to a real call site, or RETIRE it with a note saying where \
         the rule actually lives. Adding it to DEAD_CONST_ALLOWLIST is a THIRD option that \
         needs a reviewer to say why shipping a constant that enforces nothing is better than \
         not declaring one:\n  {}",
        new_dead.join("\n  ")
    );
}

#[test]
fn the_allowlist_may_only_shrink() {
    let text = fs::read_to_string(constants_path()).expect("read constants.rs");
    let declared = declared_consts(&text);
    let sources = production_sources_except_constants();

    let mut resurrected: Vec<&str> = Vec::new();
    let mut vanished: Vec<&str> = Vec::new();
    for name in DEAD_CONST_ALLOWLIST {
        if !declared.contains(*name) {
            vanished.push(name);
        } else if references(&sources, name) {
            resurrected.push(name);
        }
    }

    assert!(
        resurrected.is_empty() && vanished.is_empty(),
        "DEAD_CONST_ALLOWLIST is STALE, and a stale allowlist is not a ratchet — every entry \
         that no longer applies is a free slot for the next dead constant, which makes the \
         count look like progress while it stands still.\n\nNow WIRED (delete from the \
         allowlist — this is the good outcome):\n  {}\n\nNo longer declared (delete from the \
         allowlist):\n  {}",
        if resurrected.is_empty() {
            "(none)".to_string()
        } else {
            resurrected.join("\n  ")
        },
        if vanished.is_empty() {
            "(none)".to_string()
        } else {
            vanished.join("\n  ")
        },
    );
}

#[test]
fn guard_self_test_word_boundary_and_parser() {
    // The scan must not let a PREFIX match pass for a reference — that would
    // silently mark a dead constant alive.
    assert!(references(
        "let x = MAX_PLAUSIBLE_LTP;",
        "MAX_PLAUSIBLE_LTP"
    ));
    assert!(references("MAX_PLAUSIBLE_LTP", "MAX_PLAUSIBLE_LTP"));
    assert!(
        !references("MAX_PLAUSIBLE_LTP_CEILING", "MAX_PLAUSIBLE_LTP"),
        "a longer identifier must NOT count as a reference to the shorter one"
    );
    assert!(!references("PRE_MAX_PLAUSIBLE_LTP", "MAX_PLAUSIBLE_LTP"));
    assert!(!references("nothing here", "MAX_PLAUSIBLE_LTP"));

    let sample =
        "pub const FOO_BAR: u64 = 1;\n/// pub const NOT_THIS: u8 = 0;\npub const B: f32 = 2.0;\n";
    let got = declared_consts(sample);
    assert!(got.contains("FOO_BAR"), "must parse a normal declaration");
    assert!(got.contains("B"), "must parse a single-letter name");
    assert!(
        !got.contains("NOT_THIS"),
        "a doc-comment mention is not a declaration — only a line STARTING with `pub const` is"
    );
}
