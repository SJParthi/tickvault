//! Configuration structs deserialized from `config/base.toml`.
//!
//! Every runtime value that might differ between environments lives here.
//! Secrets are NEVER in config — they come from AWS SSM Parameter Store.

use anyhow::{Result, bail};
use chrono::NaiveTime;
use serde::Deserialize;

use crate::constants::SEBI_MAX_ORDERS_PER_SECOND;

/// Root application configuration.
#[derive(Debug, Deserialize)]
pub struct ApplicationConfig {
    pub trading: TradingConfig,
    pub dhan: DhanConfig,
    pub websocket: WebSocketConfig,
    pub questdb: QuestDbConfig,
    pub valkey: ValkeyConfig,
    pub prometheus: PrometheusConfig,
    pub network: NetworkConfig,
    pub token: TokenConfig,
    pub risk: RiskConfig,
    pub logging: LoggingConfig,
    pub instrument: InstrumentConfig,
    pub api: ApiConfig,
    pub pipeline: PipelineConfig,
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
#[derive(Debug, Clone, Deserialize)]
pub struct DhanConfig {
    /// WebSocket V2 binary feed URL.
    pub websocket_url: String,
    /// REST API base URL (for trading, data, renewal).
    pub rest_api_base_url: String,
    /// Auth base URL (for token generation — separate from REST API).
    /// Dhan uses `https://auth.dhan.co` for authentication endpoints.
    pub auth_base_url: String,
    /// Primary instrument CSV download URL.
    pub instrument_csv_url: String,
    /// Fallback instrument CSV download URL.
    pub instrument_csv_fallback_url: String,
    /// Maximum instruments per WebSocket connection.
    pub max_instruments_per_connection: usize,
    /// Maximum concurrent WebSocket connections.
    pub max_websocket_connections: usize,
}

/// WebSocket keep-alive and reconnection configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct WebSocketConfig {
    /// Expected server ping interval in seconds (Dhan server pings every 10s).
    pub ping_interval_secs: u64,
    /// Server disconnects after this many seconds with no pong.
    pub pong_timeout_secs: u64,
    /// Reserved for future use (server ping monitoring).
    pub max_consecutive_pong_failures: u32,
    /// Initial reconnection delay in milliseconds.
    pub reconnect_initial_delay_ms: u64,
    /// Maximum reconnection delay in milliseconds.
    pub reconnect_max_delay_ms: u64,
    /// Maximum reconnection attempts before giving up.
    pub reconnect_max_attempts: u32,
    /// Maximum instruments per subscription message (Dhan limit: 100).
    pub subscription_batch_size: usize,
}

/// QuestDB connection configuration.
#[derive(Debug, Clone, Deserialize)]
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

/// API server configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiConfig {
    /// Bind address for the HTTP server.
    pub host: String,
    /// HTTP server port.
    pub port: u16,
}

/// Pipeline processing configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct PipelineConfig {
    /// Broadcast channel capacity for candle updates to WebSocket clients.
    pub candle_broadcast_capacity: usize,
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

// ---------------------------------------------------------------------------
// Configuration Validation
// ---------------------------------------------------------------------------

impl ApplicationConfig {
    /// Validates all configuration values at startup.
    ///
    /// Catches invalid time formats, SEBI violations, and nonsensical ranges
    /// before the system starts trading.
    ///
    /// # Errors
    /// Returns descriptive error for the first validation failure found.
    pub fn validate(&self) -> Result<()> {
        // Timezone must be Asia/Kolkata — SEBI requirement.
        if self.trading.timezone != "Asia/Kolkata" {
            bail!(
                "trading.timezone must be 'Asia/Kolkata', got '{}'",
                self.trading.timezone
            );
        }

        // Validate time string formats.
        let time_fields = [
            ("trading.market_open_time", &self.trading.market_open_time),
            ("trading.market_close_time", &self.trading.market_close_time),
            ("trading.order_cutoff_time", &self.trading.order_cutoff_time),
            (
                "trading.data_collection_start",
                &self.trading.data_collection_start,
            ),
            (
                "trading.data_collection_end",
                &self.trading.data_collection_end,
            ),
            (
                "instrument.daily_download_time",
                &self.instrument.daily_download_time,
            ),
        ];
        for (field_name, value) in &time_fields {
            NaiveTime::parse_from_str(value, "%H:%M:%S").map_err(|_| {
                anyhow::anyhow!("{} is not a valid HH:MM:SS time: '{}'", field_name, value)
            })?;
        }

        // SEBI: max_orders_per_second must not exceed the SEBI limit.
        if self.trading.max_orders_per_second > SEBI_MAX_ORDERS_PER_SECOND {
            bail!(
                "trading.max_orders_per_second ({}) exceeds SEBI limit ({})",
                self.trading.max_orders_per_second,
                SEBI_MAX_ORDERS_PER_SECOND
            );
        }

        // Token: refresh_before_expiry must be less than token validity.
        if self.token.refresh_before_expiry_hours >= self.token.token_validity_hours {
            bail!(
                "token.refresh_before_expiry_hours ({}) must be less than token.token_validity_hours ({})",
                self.token.refresh_before_expiry_hours,
                self.token.token_validity_hours
            );
        }

        // Network: timeouts and retries must be positive.
        if self.network.request_timeout_ms == 0 {
            bail!("network.request_timeout_ms must be > 0");
        }
        if self.network.retry_max_attempts == 0 {
            bail!("network.retry_max_attempts must be > 0");
        }

        // Risk: daily loss must be positive and reasonable.
        if self.risk.max_daily_loss_percent <= 0.0 || self.risk.max_daily_loss_percent > 100.0 {
            bail!(
                "risk.max_daily_loss_percent must be in (0, 100], got {}",
                self.risk.max_daily_loss_percent
            );
        }

        // Instrument: download timeout must be positive.
        if self.instrument.csv_download_timeout_secs == 0 {
            bail!("instrument.csv_download_timeout_secs must be > 0");
        }

        // WebSocket: ping interval must be positive.
        if self.websocket.ping_interval_secs == 0 {
            bail!("websocket.ping_interval_secs must be > 0");
        }

        // WebSocket: subscription batch must be in [1, 100] (Dhan limit).
        if self.websocket.subscription_batch_size == 0
            || self.websocket.subscription_batch_size > crate::constants::SUBSCRIPTION_BATCH_SIZE
        {
            bail!(
                "websocket.subscription_batch_size must be in [1, {}], got {}",
                crate::constants::SUBSCRIPTION_BATCH_SIZE,
                self.websocket.subscription_batch_size
            );
        }

        // WebSocket: reconnect attempts must be positive.
        if self.websocket.reconnect_max_attempts == 0 {
            bail!("websocket.reconnect_max_attempts must be > 0");
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: creates a valid ApplicationConfig for testing.
    /// Modify individual fields to test specific validation failures.
    fn make_valid_config() -> ApplicationConfig {
        ApplicationConfig {
            trading: TradingConfig {
                market_open_time: "09:15:00".to_string(),
                market_close_time: "15:30:00".to_string(),
                order_cutoff_time: "15:29:00".to_string(),
                data_collection_start: "09:00:00".to_string(),
                data_collection_end: "16:00:00".to_string(),
                timezone: "Asia/Kolkata".to_string(),
                max_orders_per_second: 10,
            },
            dhan: DhanConfig {
                websocket_url: "wss://api-feed.dhan.co".to_string(),
                rest_api_base_url: "https://api.dhan.co/v2".to_string(),
                auth_base_url: "https://auth.dhan.co".to_string(),
                instrument_csv_url: "https://images.dhan.co/api-data/api-scrip-master-detailed.csv"
                    .to_string(),
                instrument_csv_fallback_url: "https://images.dhan.co/api-data/api-scrip-master.csv"
                    .to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
            },
            websocket: WebSocketConfig {
                ping_interval_secs: 10,
                pong_timeout_secs: 10,
                max_consecutive_pong_failures: 2,
                reconnect_initial_delay_ms: 500,
                reconnect_max_delay_ms: 30000,
                reconnect_max_attempts: 10,
                subscription_batch_size: 100,
            },
            questdb: QuestDbConfig {
                host: "dlt-questdb".to_string(),
                http_port: 9000,
                pg_port: 8812,
                ilp_port: 9009,
            },
            valkey: ValkeyConfig {
                host: "dlt-valkey".to_string(),
                port: 6379,
                max_connections: 16,
            },
            prometheus: PrometheusConfig {
                host: "dlt-prometheus".to_string(),
                port: 9090,
            },
            network: NetworkConfig {
                request_timeout_ms: 5000,
                websocket_connect_timeout_ms: 10000,
                retry_initial_delay_ms: 100,
                retry_max_delay_ms: 30000,
                retry_max_attempts: 5,
            },
            token: TokenConfig {
                refresh_before_expiry_hours: 1,
                token_validity_hours: 24,
            },
            risk: RiskConfig {
                max_daily_loss_percent: 2.0,
                max_position_size_lots: 100,
            },
            logging: LoggingConfig {
                level: "info".to_string(),
                format: "json".to_string(),
            },
            instrument: InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/dlt-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
            },
            api: ApiConfig {
                host: "0.0.0.0".to_string(),
                port: 3001,
            },
            pipeline: PipelineConfig {
                candle_broadcast_capacity: 4096,
            },
        }
    }

    #[test]
    fn test_valid_config_passes_validation() {
        let config = make_valid_config();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_wrong_timezone_fails() {
        let mut config = make_valid_config();
        config.trading.timezone = "UTC".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("Asia/Kolkata"));
    }

    #[test]
    fn test_invalid_time_format_fails() {
        let mut config = make_valid_config();
        config.trading.market_open_time = "9:15".to_string(); // Missing seconds
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("market_open_time"));
    }

    #[test]
    fn test_sebi_order_limit_exceeded_fails() {
        let mut config = make_valid_config();
        config.trading.max_orders_per_second = 11; // SEBI limit is 10
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("SEBI"));
    }

    #[test]
    fn test_sebi_order_limit_at_boundary_passes() {
        let mut config = make_valid_config();
        config.trading.max_orders_per_second = 10; // Exactly at limit
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_token_refresh_exceeds_validity_fails() {
        let mut config = make_valid_config();
        config.token.refresh_before_expiry_hours = 25;
        config.token.token_validity_hours = 24;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("refresh_before_expiry_hours"));
    }

    #[test]
    fn test_token_refresh_equals_validity_fails() {
        let mut config = make_valid_config();
        config.token.refresh_before_expiry_hours = 24;
        config.token.token_validity_hours = 24;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("refresh_before_expiry_hours"));
    }

    #[test]
    fn test_zero_request_timeout_fails() {
        let mut config = make_valid_config();
        config.network.request_timeout_ms = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("request_timeout_ms"));
    }

    #[test]
    fn test_zero_retry_attempts_fails() {
        let mut config = make_valid_config();
        config.network.retry_max_attempts = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("retry_max_attempts"));
    }

    #[test]
    fn test_zero_daily_loss_percent_fails() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = 0.0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_daily_loss_percent"));
    }

    #[test]
    fn test_negative_daily_loss_percent_fails() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = -5.0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_daily_loss_percent"));
    }

    #[test]
    fn test_daily_loss_percent_over_100_fails() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = 100.1;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_daily_loss_percent"));
    }

    #[test]
    fn test_daily_loss_percent_at_100_passes() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = 100.0;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_zero_csv_download_timeout_fails() {
        let mut config = make_valid_config();
        config.instrument.csv_download_timeout_secs = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("csv_download_timeout_secs"));
    }

    #[test]
    fn test_invalid_instrument_download_time_fails() {
        let mut config = make_valid_config();
        config.instrument.daily_download_time = "not-a-time".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("daily_download_time"));
    }

    // --- WebSocket Config Validation ---

    #[test]
    fn test_websocket_zero_ping_interval_fails() {
        let mut config = make_valid_config();
        config.websocket.ping_interval_secs = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("ping_interval_secs"));
    }

    #[test]
    fn test_websocket_zero_subscription_batch_size_fails() {
        let mut config = make_valid_config();
        config.websocket.subscription_batch_size = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("subscription_batch_size"));
    }

    #[test]
    fn test_websocket_subscription_batch_size_over_100_fails() {
        let mut config = make_valid_config();
        config.websocket.subscription_batch_size = 101;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("subscription_batch_size"));
    }

    #[test]
    fn test_websocket_zero_reconnect_max_attempts_fails() {
        let mut config = make_valid_config();
        config.websocket.reconnect_max_attempts = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("reconnect_max_attempts"));
    }

    #[test]
    fn test_websocket_subscription_batch_size_exactly_100_passes() {
        let mut config = make_valid_config();
        config.websocket.subscription_batch_size = 100;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_websocket_subscription_batch_size_exactly_1_passes() {
        let mut config = make_valid_config();
        config.websocket.subscription_batch_size = 1;
        assert!(config.validate().is_ok());
    }
}
