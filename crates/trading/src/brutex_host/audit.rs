//! The audit log: one NDJSON line per load, refusal, swap or keep-previous.
//! Cold; one write and one `sync_data` per event.

// O(1) EXEMPT: begin — cold path: runs once before the market opens (or in the dry-run CLI), never per tick. The strategy crate behind `engine::StrategyEngine` carries the hot-path contract (R4–R6).
use std::io::Write as _;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    Loaded,
    Refused,
    KeptPrevious,
    Swapped,
    SwapRefused,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AuditEvent {
    pub at_ist: String,
    pub kind: AuditKind,
    pub strategy: String,
    pub version: Option<u32>,
    pub params_date: Option<String>,
    pub universe_date: Option<String>,
    pub detail: String,
}

impl AuditEvent {
    /// One JSON line, no trailing newline.
    #[must_use]
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            format!(
                "{{\"kind\":\"encode_failed\",\"strategy\":{:?}}}",
                self.strategy
            )
        })
    }
}

/// Appends events to `path` (creating it), one line each, then syncs.
pub fn append(path: &Path, events: &[AuditEvent]) -> std::io::Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut buf = String::new();
    for e in events {
        buf.push_str(&e.to_line());
        buf.push('\n');
    }
    f.write_all(buf.as_bytes())?;
    f.sync_data()
}

/// Receives one alert per refusal that leaves a previous version or snapshot
/// live (decision 4). The production sink (Telegram, once per refusal per day)
/// lands with its noise-lock row; until then refusals are audited and logged.
pub trait AlertSink {
    fn refused(&mut self, strategy: &str, what: &str, reason: &str);
}

/// Collects alerts (tests and the dry run).
#[derive(Debug, Default)]
pub struct CollectAlerts {
    pub alerts: Vec<String>,
}

impl AlertSink for CollectAlerts {
    fn refused(&mut self, strategy: &str, what: &str, reason: &str) {
        self.alerts.push(format!("{strategy} {what}: {reason}"));
    }
}
// O(1) EXEMPT: end
