//! Configuration structs deserialized from `config/base.toml`.
//!
//! Every runtime value that might differ between environments lives here.
//! Secrets are NEVER in config — they come from AWS SSM Parameter Store.

use serde::Deserialize;

/// Root application configuration.
#[derive(Debug, Deserialize)]
pub struct ApplicationConfig {
    pub trading: TradingConfig,
    pub dhan: DhanConfig,
    pub questdb: QuestDbConfig,
    pub valkey: ValkeyConfig,
    pub prometheus: PrometheusConfig,
    pub network: NetworkConfig,
    pub token: TokenConfig,
    pub risk: RiskConfig,
    pub logging: LoggingConfig,
    pub instrument: InstrumentConfig,
}

/// Trading session timing configuration.
#[derive(Debug, Deserialize)]
pub struct TradingConfig {
    /// Market open time in IST (e.g., "09:15:00").
    pub market_open_time: String,
    /// Market close time in IST (e.g., "15:30:00").
    pub market_close_time: String,
    /// Order submission cutoff in IST (e.g., "15:29:00").
    pub order_cutoff_time: String,
    /// Data collection start time in IST (e.g., "09:00:00").
    pub data_collection_start: String,
    /// Data collection end time in IST (e.g., "16:00:00").
    pub data_collection_end: String,
    /// Timezone identifier (always "Asia/Kolkata").
    pub timezone: String,
    /// Maximum orders per second (SEBI limit).
    pub max_orders_per_second: u32,
}

/// Dhan API and WebSocket connection configuration.
#[derive(Debug, Deserialize)]
pub struct DhanConfig {
    /// WebSocket V2 binary feed URL.
    pub websocket_url: String,
    /// REST API base URL.
    pub rest_api_base_url: String,
    /// Primary instrument CSV download URL.
    pub instrument_csv_url: String,
    /// Fallback instrument CSV download URL.
    pub instrument_csv_fallback_url: String,
    /// Maximum instruments per WebSocket connection.
    pub max_instruments_per_connection: usize,
    /// Maximum concurrent WebSocket connections.
    pub max_websocket_connections: usize,
}

/// QuestDB connection configuration.
#[derive(Debug, Deserialize)]
pub struct QuestDbConfig {
    /// QuestDB Docker hostname.
    pub host: String,
    /// HTTP API port (web console + REST).
    pub http_port: u16,
    /// PostgreSQL wire protocol port.
    pub pg_port: u16,
    /// InfluxDB Line Protocol port (high-speed ingestion).
    pub ilp_port: u16,
}

/// Valkey (cache) connection configuration.
#[derive(Debug, Deserialize)]
pub struct ValkeyConfig {
    /// Valkey Docker hostname.
    pub host: String,
    /// Valkey port.
    pub port: u16,
    /// Maximum connections in the connection pool.
    pub max_connections: u32,
}

/// Prometheus metrics endpoint configuration.
#[derive(Debug, Deserialize)]
pub struct PrometheusConfig {
    /// Prometheus Docker hostname.
    pub host: String,
    /// Prometheus port.
    pub port: u16,
}

/// Network timeout and retry configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct NetworkConfig {
    /// HTTP request timeout in milliseconds.
    pub request_timeout_ms: u64,
    /// WebSocket connection timeout in milliseconds.
    pub websocket_connect_timeout_ms: u64,
    /// Initial retry delay in milliseconds (exponential backoff).
    pub retry_initial_delay_ms: u64,
    /// Maximum retry delay in milliseconds.
    pub retry_max_delay_ms: u64,
    /// Maximum number of retry attempts.
    pub retry_max_attempts: u32,
}

/// JWT token lifecycle configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenConfig {
    /// Hours before expiry to trigger token refresh.
    pub refresh_before_expiry_hours: u64,
    /// Token validity duration in hours.
    pub token_validity_hours: u64,
}

/// Risk management configuration.
#[derive(Debug, Deserialize)]
pub struct RiskConfig {
    /// Maximum allowed daily loss as percentage of capital.
    pub max_daily_loss_percent: f64,
    /// Maximum position size in lots.
    pub max_position_size_lots: u32,
}

/// Logging configuration.
#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    /// Log level filter (trace, debug, info, warn, error).
    pub level: String,
    /// Log output format (json, pretty).
    pub format: String,
}

/// Instrument CSV download and universe build configuration.
#[derive(Debug, Deserialize)]
pub struct InstrumentConfig {
    /// Time of day (IST) to download fresh instrument CSV. Format: "HH:MM:SS".
    pub daily_download_time: String,
    /// Directory path for caching the last successful CSV download.
    pub csv_cache_directory: String,
    /// Cached CSV filename.
    pub csv_cache_filename: String,
    /// Download timeout in seconds (overrides network timeout for this large file).
    pub csv_download_timeout_secs: u64,
}
