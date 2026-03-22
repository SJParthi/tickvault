//! Dhan Option Chain API request/response types.
//!
//! All field names match the Dhan API exactly. PascalCase for requests
//! (via `#[serde(rename)]`), snake_case for responses.
//!
//! Source: `docs/dhan-ref/06-option-chain.md`

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request Types
// ---------------------------------------------------------------------------

/// Request body for `POST /v2/optionchain`.
///
/// PascalCase field names required by Dhan API.
#[derive(Debug, Clone, Serialize)]
pub struct OptionChainRequest {
    /// SecurityId of the underlying (from instrument master, NOT string).
    #[serde(rename = "UnderlyingScrip")]
    pub underlying_scrip: u64,
    /// Exchange segment of underlying (e.g., "IDX_I", "NSE_EQ").
    #[serde(rename = "UnderlyingSeg")]
    pub underlying_seg: String,
    /// Expiry date from expiry list API (format: "YYYY-MM-DD").
    #[serde(rename = "Expiry")]
    pub expiry: String,
}

/// Request body for `POST /v2/optionchain/expirylist`.
#[derive(Debug, Clone, Serialize)]
pub struct ExpiryListRequest {
    /// SecurityId of the underlying.
    #[serde(rename = "UnderlyingScrip")]
    pub underlying_scrip: u64,
    /// Exchange segment of underlying.
    #[serde(rename = "UnderlyingSeg")]
    pub underlying_seg: String,
}

// ---------------------------------------------------------------------------
// Response Types
// ---------------------------------------------------------------------------

/// Response from `POST /v2/optionchain`.
#[derive(Debug, Clone, Deserialize)]
pub struct OptionChainResponse {
    /// Option chain data.
    pub data: OptionChainData,
    /// "success" or error status.
    pub status: String,
}

/// Option chain data containing spot price and strike-wise options.
#[derive(Debug, Clone, Deserialize)]
pub struct OptionChainData {
    /// Current underlying LTP (spot price).
    pub last_price: f64,
    /// Strike-wise option data. Key = strike price as decimal string (e.g., "25650.000000").
    pub oc: HashMap<String, StrikeData>,
}

/// Data for a single strike price (both CE and PE sides).
#[derive(Debug, Clone, Deserialize)]
pub struct StrikeData {
    /// Call option data (None for deep OTM strikes with no data).
    pub ce: Option<OptionData>,
    /// Put option data (None for deep OTM strikes with no data).
    pub pe: Option<OptionData>,
}

/// Data for a single option contract (CE or PE).
#[derive(Debug, Clone, Deserialize)]
pub struct OptionData {
    /// VWAP for the day.
    pub average_price: f64,
    /// Dhan-computed Greeks.
    pub greeks: DhanGreeks,
    /// Implied volatility (as percentage, e.g., 9.789 = 9.789%).
    pub implied_volatility: f64,
    /// Last traded price.
    pub last_price: f64,
    /// Current open interest.
    pub oi: i64,
    /// Previous day close price.
    pub previous_close_price: f64,
    /// Previous day open interest.
    pub previous_oi: i64,
    /// Previous day volume.
    pub previous_volume: i64,
    /// SecurityId of this option contract (for WS subscription / order placement).
    pub security_id: u64,
    /// Best ask price.
    pub top_ask_price: f64,
    /// Quantity at best ask.
    pub top_ask_quantity: i64,
    /// Best bid price.
    pub top_bid_price: f64,
    /// Quantity at best bid.
    pub top_bid_quantity: i64,
    /// Today's volume.
    pub volume: i64,
}

/// Dhan-provided Greeks (from their Black-Scholes computation).
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct DhanGreeks {
    /// Delta: rate of change of option price per Rs.1 underlying move.
    pub delta: f64,
    /// Theta: time decay per day (negative for long options).
    pub theta: f64,
    /// Gamma: rate of change of delta.
    pub gamma: f64,
    /// Vega: sensitivity to 1% IV change.
    pub vega: f64,
}

/// Response from `POST /v2/optionchain/expirylist`.
#[derive(Debug, Clone, Deserialize)]
pub struct ExpiryListResponse {
    /// List of active expiry dates in "YYYY-MM-DD" format.
    pub data: Vec<String>,
    /// "success" or error status.
    pub status: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_option_chain_request_serializes_pascal_case() {
        let req = OptionChainRequest {
            underlying_scrip: 13,
            underlying_seg: "IDX_I".to_string(),
            expiry: "2024-10-31".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"UnderlyingScrip\":13"));
        assert!(json.contains("\"UnderlyingSeg\":\"IDX_I\""));
        assert!(json.contains("\"Expiry\":\"2024-10-31\""));
    }

    #[test]
    fn test_expiry_list_request_serializes_pascal_case() {
        let req = ExpiryListRequest {
            underlying_scrip: 13,
            underlying_seg: "IDX_I".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"UnderlyingScrip\":13"));
        assert!(json.contains("\"UnderlyingSeg\":\"IDX_I\""));
    }

    #[test]
    fn test_option_chain_response_deserializes() {
        let json = r#"{
            "data": {
                "last_price": 25642.8,
                "oc": {
                    "25650.000000": {
                        "ce": {
                            "average_price": 146.99,
                            "greeks": { "delta": 0.53871, "theta": -15.1539, "gamma": 0.00132, "vega": 12.18593 },
                            "implied_volatility": 9.789,
                            "last_price": 134.0,
                            "oi": 3786445,
                            "previous_close_price": 244.85,
                            "previous_oi": 402220,
                            "previous_volume": 31931705,
                            "security_id": 42528,
                            "top_ask_price": 134.0,
                            "top_ask_quantity": 1365,
                            "top_bid_price": 133.55,
                            "top_bid_quantity": 1625,
                            "volume": 117567970
                        },
                        "pe": null
                    }
                }
            },
            "status": "success"
        }"#;

        let resp: OptionChainResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.status, "success");
        assert!((resp.data.last_price - 25642.8).abs() < 0.01);

        let strike = resp.data.oc.get("25650.000000").unwrap();
        let ce = strike.ce.as_ref().unwrap();
        assert_eq!(ce.security_id, 42528);
        assert!((ce.greeks.delta - 0.53871).abs() < 0.0001);
        assert!((ce.greeks.theta - (-15.1539)).abs() < 0.001);
        assert!((ce.greeks.gamma - 0.00132).abs() < 0.00001);
        assert!((ce.greeks.vega - 12.18593).abs() < 0.001);
        assert!((ce.implied_volatility - 9.789).abs() < 0.001);
        assert_eq!(ce.oi, 3786445);
        assert!(
            strike.pe.is_none(),
            "PE should be None when null in response"
        );
    }

    #[test]
    fn test_option_chain_response_both_ce_pe() {
        let json = r#"{
            "data": {
                "last_price": 23000.0,
                "oc": {
                    "23000.000000": {
                        "ce": {
                            "average_price": 200.0,
                            "greeks": { "delta": 0.55, "theta": -10.0, "gamma": 0.001, "vega": 8.0 },
                            "implied_volatility": 15.0,
                            "last_price": 190.0,
                            "oi": 500000,
                            "previous_close_price": 180.0,
                            "previous_oi": 450000,
                            "previous_volume": 1000000,
                            "security_id": 12345,
                            "top_ask_price": 191.0,
                            "top_ask_quantity": 100,
                            "top_bid_price": 189.0,
                            "top_bid_quantity": 200,
                            "volume": 2000000
                        },
                        "pe": {
                            "average_price": 180.0,
                            "greeks": { "delta": -0.45, "theta": -9.5, "gamma": 0.001, "vega": 8.0 },
                            "implied_volatility": 16.0,
                            "last_price": 170.0,
                            "oi": 600000,
                            "previous_close_price": 160.0,
                            "previous_oi": 550000,
                            "previous_volume": 800000,
                            "security_id": 12346,
                            "top_ask_price": 171.0,
                            "top_ask_quantity": 150,
                            "top_bid_price": 169.0,
                            "top_bid_quantity": 250,
                            "volume": 1500000
                        }
                    }
                }
            },
            "status": "success"
        }"#;

        let resp: OptionChainResponse = serde_json::from_str(json).unwrap();
        let strike = resp.data.oc.get("23000.000000").unwrap();
        assert!(strike.ce.is_some(), "CE should be present");
        assert!(strike.pe.is_some(), "PE should be present");
        let pe = strike.pe.as_ref().unwrap();
        assert_eq!(pe.security_id, 12346);
        assert!((pe.greeks.delta - (-0.45)).abs() < 0.01);
    }

    #[test]
    fn test_expiry_list_response_deserializes() {
        let json = r#"{"data": ["2024-10-17", "2024-10-24", "2024-10-31"], "status": "success"}"#;
        let resp: ExpiryListResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.data.len(), 3);
        assert_eq!(resp.data[0], "2024-10-17");
    }

    #[test]
    fn test_strike_key_decimal_format() {
        // Dhan uses decimal string keys like "25650.000000".
        let json = r#"{"data":{"last_price":100.0,"oc":{"25650.000000":{"ce":null,"pe":null}}},"status":"success"}"#;
        let resp: OptionChainResponse = serde_json::from_str(json).unwrap();
        assert!(resp.data.oc.contains_key("25650.000000"));
        // Parse strike as f64.
        let strike_str = "25650.000000";
        let strike_val: f64 = strike_str.parse().unwrap();
        assert!((strike_val - 25650.0).abs() < 0.01);
    }

    #[test]
    fn test_dhan_greeks_is_copy() {
        let g = DhanGreeks {
            delta: 0.5,
            theta: -10.0,
            gamma: 0.001,
            vega: 8.0,
        };
        let g2 = g; // Copy
        assert_eq!(g.delta, g2.delta);
    }

    #[test]
    fn test_option_chain_request_clone() {
        let req = OptionChainRequest {
            underlying_scrip: 13,
            underlying_seg: "IDX_I".to_string(),
            expiry: "2024-10-31".to_string(),
        };
        let req2 = req.clone();
        assert_eq!(req.underlying_scrip, req2.underlying_scrip);
    }
}
