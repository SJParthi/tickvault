// Compile-time lint enforcement — defense-in-depth with CLI clippy flags
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![allow(missing_docs)]

//! Application boot helpers — extracted from main.rs for testability.
//!
//! Pure functions that can be unit-tested without network/external dependencies.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{FixedOffset, Utc};
use figment::Figment;
use figment::providers::{Format, Toml};
use secrecy::ExposeSecret;
use tracing::{error, info, warn};

use dhan_live_trader_common::config::ApplicationConfig;
use dhan_live_trader_common::constants::IST_UTC_OFFSET_SECONDS;
use dhan_live_trader_core::auth::secret_manager;
use dhan_live_trader_core::auth::token_manager::TokenManager;
use dhan_live_trader_core::instrument::{build_fno_universe, build_subscription_plan};
use dhan_live_trader_core::notification::{NotificationEvent, NotificationService};
use dhan_live_trader_core::pipeline::run_tick_processor;
use dhan_live_trader_core::websocket::connection_pool::WebSocketConnectionPool;
use dhan_live_trader_core::websocket::types::InstrumentSubscription;

use dhan_live_trader_storage::tick_persistence::{
    TickPersistenceWriter, ensure_tick_table_dedup_keys,
};

use dhan_live_trader_api::build_router;
use dhan_live_trader_api::state::SharedAppState;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Base config file path (relative to working directory).
pub const CONFIG_BASE_PATH: &str = "config/base.toml";

/// Local override config file path (git-ignored, optional).
pub const CONFIG_LOCAL_PATH: &str = "config/local.toml";

// ---------------------------------------------------------------------------
// Config Loading
// ---------------------------------------------------------------------------

/// Loads and validates the application configuration from TOML files.
///
/// Merges `config/base.toml` with optional `config/local.toml` overrides.
///
/// # Errors
///
/// Returns error if the config file cannot be read, parsed, or fails validation.
pub fn load_config() -> Result<ApplicationConfig> {
    let config: ApplicationConfig = Figment::new()
        .merge(Toml::file(CONFIG_BASE_PATH))
        .merge(Toml::file(CONFIG_LOCAL_PATH))
        .extract()
        .context("failed to load configuration from config/base.toml")?;

    config
        .validate()
        .context("configuration validation failed")?;

    Ok(config)
}

/// Parses a host:port string into a `SocketAddr`.
///
/// # Errors
///
/// Returns error if the address cannot be parsed.
pub fn parse_bind_address(host: &str, port: u16) -> Result<SocketAddr> {
    let addr_str = format!("{host}:{port}");
    addr_str.parse().context("invalid API bind address")
}

/// Returns the mode string based on whether a token manager is available.
pub fn determine_mode(has_token: bool) -> &'static str {
    if has_token { "LIVE" } else { "OFFLINE" }
}

/// A type-erased task handle that can be aborted.
pub struct AbortHandle(Box<dyn AbortableHandle + Send>);

trait AbortableHandle {
    fn abort(&self);
}

impl<T: Send + 'static> AbortableHandle for tokio::task::JoinHandle<T> {
    fn abort(&self) {
        tokio::task::JoinHandle::abort(self);
    }
}

impl AbortHandle {
    /// Wraps any `JoinHandle<T>` into an `AbortHandle`.
    pub fn new<T: Send + 'static>(handle: tokio::task::JoinHandle<T>) -> Self {
        Self(Box::new(handle))
    }

    /// Aborts the underlying task.
    pub fn abort(&self) {
        self.0.abort();
    }
}

/// Represents the handles of background tasks that need cleanup on shutdown.
pub struct BackgroundHandles {
    /// Token renewal task handle.
    pub renewal: Option<AbortHandle>,
    /// Tick processor task handle.
    pub processor: Option<AbortHandle>,
    /// API server task handle.
    pub api: AbortHandle,
    /// WebSocket connection handles.
    pub websocket: Vec<AbortHandle>,
}

impl BackgroundHandles {
    /// Aborts all background tasks for graceful shutdown.
    pub fn abort_all(self) {
        if let Some(handle) = &self.renewal {
            handle.abort();
        }
        if let Some(handle) = &self.processor {
            handle.abort();
        }
        self.api.abort();
        for handle in &self.websocket {
            handle.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Tracing
// ---------------------------------------------------------------------------

/// Initializes the tracing subscriber with the given log level and format.
///
/// Uses `try_init` so repeated calls (e.g. from tests) are harmless.
pub fn init_tracing(level: &str, format: &str) {
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .with_target(true)
        .with_thread_ids(true)
        .with_file(false)
        .with_line_number(false);

    if format == "json" {
        let _ = subscriber.json().try_init();
    } else {
        let _ = subscriber.try_init();
    }
}

/// Installs the rustls CryptoProvider. Safe to call multiple times.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

// ---------------------------------------------------------------------------
// Boot Sequence
// ---------------------------------------------------------------------------

/// Result of the boot sequence, containing handles for graceful shutdown.
pub struct BootResult {
    /// Background task handles.
    pub handles: BackgroundHandles,
    /// Notification service (for shutdown notification).
    pub notifier: Arc<NotificationService>,
    /// Operating mode ("LIVE" or "OFFLINE").
    pub mode: &'static str,
    /// Bound API server address.
    pub bind_addr: SocketAddr,
}

/// Runs the complete boot sequence (steps 3–10).
///
/// Steps 0–2 (CryptoProvider, config, logging) are handled by the caller.
///
/// # Errors
///
/// Returns error if the API server fails to bind.
pub async fn boot(config: ApplicationConfig) -> Result<BootResult> {
    // Step 3: Notification (best-effort — no-op if SSM unavailable)
    info!("initializing Telegram notification service");
    let notifier = NotificationService::initialize(&config.notification).await;

    // Step 4: Authenticate (best-effort)
    info!("authenticating with Dhan API via SSM → TOTP → JWT");
    let token_manager =
        match TokenManager::initialize(&config.dhan, &config.token, &config.network).await {
            Ok(manager) => {
                info!("authentication successful — token acquired");
                notifier.notify(NotificationEvent::AuthenticationSuccess);
                Some(manager)
            }
            Err(err) => {
                warn!(
                    error = %err,
                    "authentication failed — starting in offline mode (no live data)"
                );
                notifier.notify(NotificationEvent::AuthenticationFailed {
                    reason: err.to_string(),
                });
                None
            }
        };

    // Step 5: QuestDB persistence (best-effort)
    info!("setting up QuestDB tick persistence");
    ensure_tick_table_dedup_keys(&config.questdb).await;

    let tick_writer = match TickPersistenceWriter::new(&config.questdb) {
        Ok(writer) => {
            info!("QuestDB tick writer connected");
            Some(writer)
        }
        Err(err) => {
            warn!(
                ?err,
                "QuestDB tick writer unavailable — ticks will not be persisted"
            );
            None
        }
    };

    // Step 6: F&O universe + subscription plan (best-effort)
    info!("building F&O universe from instrument CSV");
    let subscription_plan = match build_fno_universe(
        &config.dhan.instrument_csv_url,
        &config.dhan.instrument_csv_fallback_url,
        &config.instrument,
    )
    .await
    {
        Ok(universe) => {
            let ist = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS)
                .context("invalid IST offset constant")?;
            let today = Utc::now().with_timezone(&ist).date_naive();
            let plan = build_subscription_plan(&universe, &config.subscription, today);

            info!(
                major_indices = plan.summary.major_index_values,
                display_indices = plan.summary.display_indices,
                index_derivatives = plan.summary.index_derivatives,
                stock_equities = plan.summary.stock_equities,
                stock_derivatives = plan.summary.stock_derivatives,
                total = plan.summary.total,
                feed_mode = %plan.summary.feed_mode,
                "subscription plan ready"
            );
            Some(plan)
        }
        Err(err) => {
            warn!(
                error = %err,
                "F&O universe build failed — no instruments to subscribe"
            );
            None
        }
    };

    // Step 7: WebSocket pool (only if authenticated + plan ready)
    let (frame_receiver, ws_handles) = if let (Some(tm), Some(plan)) =
        (&token_manager, &subscription_plan)
    {
        info!("building WebSocket connection pool");

        let token_handle = tm.token_handle();
        let client_id = {
            let credentials = secret_manager::fetch_dhan_credentials()
                .await
                .context("failed to fetch Dhan client ID for WebSocket")?;
            credentials.client_id.expose_secret().to_string()
        };

        let instruments: Vec<InstrumentSubscription> = plan
            .registry
            .iter()
            .map(|inst| InstrumentSubscription::new(inst.exchange_segment, inst.security_id))
            .collect();

        let feed_mode = plan.summary.feed_mode;
        let instrument_count = instruments.len();

        let mut pool = WebSocketConnectionPool::new(
            token_handle,
            client_id,
            config.dhan.clone(),
            config.websocket.clone(),
            instruments,
            feed_mode,
        )
        .context("failed to create WebSocket connection pool")?;

        let receiver = pool.take_frame_receiver();
        let handles = pool.spawn_all();

        info!(
            connections = handles.len(),
            instruments = instrument_count,
            feed_mode = %feed_mode,
            "WebSocket pool started"
        );

        let abort_handles: Vec<AbortHandle> = handles.into_iter().map(AbortHandle::new).collect();
        (Some(receiver), abort_handles)
    } else {
        warn!("WebSocket pool skipped — running in offline mode");
        (None, Vec::new())
    };

    // Step 8: Tick processor
    let processor_handle = if let Some(receiver) = frame_receiver {
        let handle = tokio::spawn(async move {
            run_tick_processor(receiver, tick_writer).await;
        });
        info!("tick processor started");
        Some(AbortHandle::new(handle))
    } else {
        info!("tick processor skipped — no frame source available");
        None
    };

    // Step 9: API server
    let api_state = SharedAppState::new(config.questdb.clone());
    let router = build_router(api_state);
    let bind_addr = parse_bind_address(&config.api.host, config.api.port)?;

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .context("failed to bind API server")?;

    let bind_addr = listener
        .local_addr()
        .context("failed to get local address")?;

    info!(
        address = %bind_addr,
        "API server listening — open http://{} in browser",
        bind_addr
    );

    let api_handle = AbortHandle::new(tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, router).await {
            error!(?err, "API server error");
        }
    }));

    // Step 10: Token renewal (only if authenticated)
    let renewal_handle = if let Some(ref tm) = token_manager {
        let handle = tm.spawn_renewal_task();
        info!("token renewal task started");
        Some(AbortHandle::new(handle))
    } else {
        None
    };

    let mode = determine_mode(token_manager.is_some());

    let handles = BackgroundHandles {
        renewal: renewal_handle,
        processor: processor_handle,
        api: api_handle,
        websocket: ws_handles,
    };

    Ok(BootResult {
        handles,
        notifier,
        mode,
        bind_addr,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: creates an AbortHandle wrapping a never-completing task.
    // Uses std::future::pending to avoid uncovered async block bodies.
    fn pending_handle() -> AbortHandle {
        AbortHandle::new(tokio::spawn(std::future::pending::<()>()))
    }

    fn pending_handle_result() -> AbortHandle {
        AbortHandle::new(tokio::spawn(std::future::pending::<
            Result<(), anyhow::Error>,
        >()))
    }

    #[test]
    fn test_config_base_path_is_config_base_toml() {
        assert_eq!(CONFIG_BASE_PATH, "config/base.toml");
    }

    #[test]
    fn test_config_local_path_is_config_local_toml() {
        assert_eq!(CONFIG_LOCAL_PATH, "config/local.toml");
    }

    #[test]
    fn test_parse_bind_address_valid_ipv4() {
        let addr = parse_bind_address("0.0.0.0", 8080).unwrap();
        assert_eq!(addr.ip().to_string(), "0.0.0.0");
        assert_eq!(addr.port(), 8080);
    }

    #[test]
    fn test_parse_bind_address_localhost() {
        let addr = parse_bind_address("127.0.0.1", 3000).unwrap();
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_eq!(addr.port(), 3000);
    }

    #[test]
    fn test_parse_bind_address_invalid_host() {
        let result = parse_bind_address("not-an-ip", 8080);
        assert!(result.is_err());
    }

    #[test]
    fn test_determine_mode_live_when_token_present() {
        assert_eq!(determine_mode(true), "LIVE");
    }

    #[test]
    fn test_determine_mode_offline_when_no_token() {
        assert_eq!(determine_mode(false), "OFFLINE");
    }

    #[test]
    fn test_load_config_succeeds_with_config_file() {
        let result = load_config();
        assert!(result.is_ok(), "load_config should succeed: {result:?}");
    }

    #[tokio::test]
    async fn test_background_handles_abort_all_no_panic() {
        let handles = BackgroundHandles {
            renewal: Some(pending_handle()),
            processor: Some(pending_handle()),
            api: pending_handle(),
            websocket: vec![pending_handle(), pending_handle()],
        };
        handles.abort_all();
    }

    #[tokio::test]
    async fn test_background_handles_abort_all_with_none() {
        let handles = BackgroundHandles {
            renewal: None,
            processor: None,
            api: pending_handle(),
            websocket: vec![],
        };
        handles.abort_all();
    }

    #[tokio::test]
    async fn test_abort_handle_new_with_result_type() {
        let abort = pending_handle_result();
        abort.abort();
    }

    #[tokio::test]
    async fn test_abort_handle_new_with_string_type() {
        let handle = tokio::spawn(std::future::pending::<String>());
        let abort = AbortHandle::new(handle);
        abort.abort();
    }

    #[tokio::test]
    async fn test_abort_handle_new_with_i32_type() {
        let handle = tokio::spawn(std::future::pending::<i32>());
        let abort = AbortHandle::new(handle);
        abort.abort();
    }

    #[tokio::test]
    async fn test_abort_handle_abort_is_idempotent() {
        let abort = pending_handle();
        abort.abort();
        abort.abort();
    }

    #[tokio::test]
    async fn test_background_handles_abort_all_with_many_websockets() {
        let mut ws_handles = Vec::new();
        for _ in 0..10 {
            ws_handles.push(pending_handle());
        }
        let handles = BackgroundHandles {
            renewal: None,
            processor: None,
            api: pending_handle(),
            websocket: ws_handles,
        };
        handles.abort_all();
    }

    #[tokio::test]
    async fn test_background_handles_abort_all_with_result_typed_handles() {
        let handles = BackgroundHandles {
            renewal: Some(pending_handle_result()),
            processor: Some(pending_handle_result()),
            api: pending_handle(),
            websocket: vec![],
        };
        handles.abort_all();
    }

    #[test]
    fn test_parse_bind_address_ipv6_unsupported_format() {
        let result = parse_bind_address("::1", 9090);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_bind_address_port_zero() {
        let addr = parse_bind_address("0.0.0.0", 0).unwrap();
        assert_eq!(addr.port(), 0);
    }

    #[test]
    fn test_init_tracing_text_format() {
        init_tracing("warn", "text");
    }

    #[test]
    fn test_init_tracing_json_format() {
        init_tracing("info", "json");
    }

    #[test]
    fn test_init_tracing_repeated_calls_no_panic() {
        init_tracing("debug", "text");
        init_tracing("error", "json");
        init_tracing("trace", "text");
    }

    #[test]
    fn test_install_crypto_provider_idempotent() {
        install_crypto_provider();
        install_crypto_provider();
    }

    #[tokio::test]
    async fn test_boot_offline_mode() {
        install_crypto_provider();
        init_tracing("warn", "text");

        let mut config = load_config().unwrap();
        // Use random port to avoid conflicts
        config.api.port = 0;
        // Point external services to unreachable addresses for fast failure
        config.questdb.host = "127.0.0.1".to_string();
        config.questdb.pg_port = 1;
        config.questdb.ilp_port = 1;
        config.questdb.http_port = 1;
        config.dhan.instrument_csv_url = "http://127.0.0.1:1/fail".to_string();
        config.dhan.instrument_csv_fallback_url = "http://127.0.0.1:1/fail".to_string();

        let boot_result = match boot(config).await {
            Ok(r) => r,
            Err(e) => panic!("boot should succeed in offline mode: {e}"),
        };
        assert_eq!(boot_result.mode, "OFFLINE");
        assert!(boot_result.bind_addr.port() > 0);
        boot_result.handles.abort_all();
    }

    #[test]
    fn test_boot_result_mode_matches_determine_mode() {
        assert_eq!(determine_mode(false), "OFFLINE");
        assert_eq!(determine_mode(true), "LIVE");
    }

    /// Build a minimal instrument CSV that passes all parse + validation checks.
    ///
    /// Mirrors `build_end_to_end_csv()` from universe_builder.rs tests.
    fn build_boot_test_csv() -> String {
        use dhan_live_trader_common::constants::{
            INSTRUMENT_CSV_MIN_BYTES, INSTRUMENT_CSV_MIN_ROWS,
        };

        let header = "EXCH_ID,SEGMENT,SECURITY_ID,ISIN,INSTRUMENT,\
                       UNDERLYING_SECURITY_ID,UNDERLYING_SYMBOL,SYMBOL_NAME,\
                       DISPLAY_NAME,INSTRUMENT_TYPE,SERIES,LOT_SIZE,\
                       SM_EXPIRY_DATE,STRIKE_PRICE,OPTION_TYPE,TICK_SIZE,\
                       EXPIRY_FLAG,";

        let estimated_rows = INSTRUMENT_CSV_MIN_ROWS + 1000;
        let mut lines: Vec<String> = Vec::with_capacity(estimated_rows + 1);
        lines.push(header.to_owned());

        // NSE Indices (Pass 1)
        let nse_indices: &[(u32, &str)] = &[
            (13, "NIFTY"),
            (25, "BANKNIFTY"),
            (27, "FINNIFTY"),
            (442, "MIDCPNIFTY"),
            (38, "NIFTY NEXT 50"),
        ];
        for &(sid, sym) in nse_indices {
            lines.push(format!(
                "NSE,I,{sid},NA,INDEX,{sid},{sym},{sym},{sym} Index,INDEX,NA,\
                 1.0,0001-01-01,,XX,0.0500,N,"
            ));
        }

        // BSE Indices (Pass 1)
        let bse_indices: &[(u32, &str, &str)] = &[
            (51, "SENSEX", "S&P BSE SENSEX"),
            (69, "BANKEX", "S&P BSE BANKEX"),
            (83, "SNSX50", "S&P BSE SENSEX 50"),
        ];
        for &(sid, sym, display) in bse_indices {
            lines.push(format!(
                "BSE,I,{sid},NA,INDEX,{sid},{sym},{sym},{display},INDEX,NA,\
                 1.0,0001-01-01,,XX,0.0100,N,"
            ));
        }

        // Must-exist equities (Pass 2)
        let must_exist_equities: &[(u32, &str)] = &[
            (2885, "RELIANCE"),
            (1333, "HDFCBANK"),
            (1594, "INFY"),
            (11536, "TCS"),
            (5258, "SBIN"),
        ];
        for &(sid, sym) in must_exist_equities {
            lines.push(format!(
                "NSE,E,{sid},INE000A00000,EQUITY,,{sym},{sym} LTD,{sym},\
                 ES,EQ,1.0,,,,5.0000,NA,"
            ));
        }

        let expiry = "2027-12-30";

        // NSE FUTIDX rows (Pass 3 — index underlyings)
        let nse_futidx: &[(u32, u32, &str, u32)] = &[
            (51700, 26000, "NIFTY", 75),
            (51701, 26009, "BANKNIFTY", 30),
            (51712, 26037, "FINNIFTY", 60),
            (51713, 26074, "MIDCPNIFTY", 120),
            (51714, 26041, "NIFTYNXT50", 40),
        ];
        for &(sid, usid, sym, lot) in nse_futidx {
            lines.push(format!(
                "NSE,D,{sid},NA,FUTIDX,{usid},{sym},{sym}-{expiry}-FUT,\
                 {sym} FUT,FUT,NA,{lot}.0,{expiry},-0.01000,XX,5.0000,M,"
            ));
        }

        // BSE FUTIDX rows
        let bse_futidx: &[(u32, u32, &str, u32)] = &[
            (60000, 1, "SENSEX", 20),
            (60001, 2, "BANKEX", 30),
            (60002, 3, "SENSEX50", 25),
        ];
        for &(sid, usid, sym, lot) in bse_futidx {
            lines.push(format!(
                "BSE,D,{sid},NA,FUTIDX,{usid},{sym},{sym}-{expiry}-FUT,\
                 {sym} FUT,FUT,NA,{lot}.0,{expiry},-0.01000,XX,5.0000,M,"
            ));
        }

        // Must-exist F&O stocks: FUTSTK rows (Pass 3)
        let must_exist_futstk: &[(u32, u32, &str, u32)] = &[
            (52023, 2885, "RELIANCE", 500),
            (52024, 1333, "HDFCBANK", 550),
            (52025, 1594, "INFY", 400),
            (52026, 11536, "TCS", 175),
            (52027, 5258, "SBIN", 1500),
        ];
        for &(sid, usid, sym, lot) in must_exist_futstk {
            lines.push(format!(
                "NSE,D,{sid},NA,FUTSTK,{usid},{sym},{sym}-{expiry}-FUT,\
                 {sym} FUT,FUT,NA,{lot}.0,{expiry},-0.01000,XX,10.0000,M,"
            ));
        }

        // 160 additional stock underlyings to exceed VALIDATION_FNO_STOCK_MIN_COUNT (150)
        let extra_stock_count: u32 = 160;
        let equity_base_sid: u32 = 20000;
        let futstk_base_sid: u32 = 55000;
        for i in 0..extra_stock_count {
            let sym = format!("STOCK{i:03}");
            let eq_sid = equity_base_sid + i;
            let fut_sid = futstk_base_sid + i;

            lines.push(format!(
                "NSE,E,{eq_sid},INE000B00000,EQUITY,,{sym},{sym} LTD,{sym},\
                 ES,EQ,1.0,,,,5.0000,NA,"
            ));
            lines.push(format!(
                "NSE,D,{fut_sid},NA,FUTSTK,{eq_sid},{sym},{sym}-{expiry}-FUT,\
                 {sym} FUT,FUT,NA,100.0,{expiry},-0.01000,XX,10.0000,M,"
            ));
        }

        // OPTIDX rows (NIFTY options for option chain coverage)
        let opt_rows: &[(u32, f64, &str)] = &[
            (70001, 22000.0, "CE"),
            (70002, 22000.0, "PE"),
            (70003, 22500.0, "CE"),
            (70004, 22500.0, "PE"),
        ];
        for &(sid, strike, ot) in opt_rows {
            lines.push(format!(
                "NSE,D,{sid},NA,OPTIDX,26000,NIFTY,NIFTY-{expiry}-{strike:.0}-{ot},\
                 NIFTY {expiry} {strike:.0} {ot},OP,NA,75.0,{expiry},\
                 {strike:.5},{ot},5.0000,M,"
            ));
        }

        // Pad with MCX rows to reach INSTRUMENT_CSV_MIN_ROWS
        let real_rows = lines.len() - 1;
        let padding_needed = if real_rows < INSTRUMENT_CSV_MIN_ROWS {
            INSTRUMENT_CSV_MIN_ROWS - real_rows + 1
        } else {
            1
        };

        for i in 0..padding_needed {
            let pad_sid = 900_000 + i as u32;
            lines.push(format!(
                "MCX,D,{pad_sid},NA,FUTCOM,500,CRUDEOIL,CRUDEOIL-FUT,\
                 CRUDE OIL FUT,FUT,NA,100.0,{expiry},-0.01,XX,1.0,M,"
            ));
        }

        let csv = lines.join("\n");

        // Pad with MCX rows if still under INSTRUMENT_CSV_MIN_BYTES
        if csv.len() < INSTRUMENT_CSV_MIN_BYTES {
            let extra_bytes = INSTRUMENT_CSV_MIN_BYTES - csv.len() + 1;
            let pad_row = "MCX,D,999999,NA,FUTCOM,500,CRUDEOIL,CRUDEOIL-FUT,\
                           CRUDE OIL FUT,FUT,NA,100.0,2027-12-30,-0.01,XX,1.0,M,";
            let rows_needed = extra_bytes / pad_row.len() + 1;
            let mut extra_lines = Vec::with_capacity(rows_needed);
            for _ in 0..rows_needed {
                extra_lines.push(pad_row.to_owned());
            }
            return format!("{}\n{}", csv, extra_lines.join("\n"));
        }

        csv
    }

    #[tokio::test]
    async fn test_boot_partial_online_mode() {
        install_crypto_provider();
        init_tracing("warn", "text");

        // TCP server for QuestDB ILP (accepts connections, drains bytes)
        let tcp_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp_port = tcp_listener.local_addr().unwrap().port();
        let _tcp_server = std::thread::spawn(move || {
            for stream in tcp_listener.incoming() {
                match stream {
                    Ok(mut s) => {
                        std::thread::spawn(move || {
                            use std::io::Read;
                            let mut buf = [0u8; 4096];
                            while s.read(&mut buf).unwrap_or(0) > 0 {}
                        });
                    }
                    Err(_) => break,
                }
            }
        });

        // Mock HTTP server for instrument CSV
        let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http_listener.local_addr().unwrap().port();
        let csv_content = build_boot_test_csv();
        let _http_server = tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = http_listener.accept().await {
                    let csv = csv_content.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = vec![0u8; 4096];
                        let _ = stream.read(&mut buf).await;

                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: text/csv\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\r\n",
                            csv.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.write_all(csv.as_bytes()).await;
                        let _ = stream.shutdown().await;
                    });
                }
            }
        });

        let mut config = load_config().unwrap();
        config.api.port = 0;
        config.questdb.host = "127.0.0.1".to_string();
        config.questdb.ilp_port = tcp_port;
        config.questdb.pg_port = 1; // unused
        config.questdb.http_port = tcp_port; // DDL requests get raw TCP (best-effort)
        config.dhan.instrument_csv_url = format!("http://127.0.0.1:{http_port}/instruments.csv");
        config.dhan.instrument_csv_fallback_url =
            format!("http://127.0.0.1:{http_port}/instruments.csv");

        let boot_result = match boot(config).await {
            Ok(r) => r,
            Err(e) => panic!("boot should succeed: {e}"),
        };

        // Mode should be OFFLINE (no auth) but QuestDB and CSV succeeded
        assert_eq!(boot_result.mode, "OFFLINE");
        assert!(boot_result.bind_addr.port() > 0);
        boot_result.handles.abort_all();
    }
}
