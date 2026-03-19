//! API contract tests — verify request/response schemas.
//!
//! These tests ensure the API endpoints return well-formed responses
//! that match the documented schema. Any change to response structure
//! breaks downstream consumers (frontend portal, Grafana dashboards).

use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use dhan_live_trader_api::build_router;
use dhan_live_trader_api::state::SharedAppState;
use dhan_live_trader_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

// ---------------------------------------------------------------------------
// Helper — build test app with mock state
// ---------------------------------------------------------------------------

fn test_state() -> SharedAppState {
    SharedAppState::new(
        QuestDbConfig {
            host: "localhost".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        },
        DhanConfig {
            websocket_url: "wss://api-feed.dhan.co".to_string(),
            order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
            rest_api_base_url: "https://api.dhan.co/v2".to_string(),
            auth_base_url: "https://auth.dhan.co".to_string(),
            instrument_csv_url: "https://images.dhan.co/api-data/api-scrip-master-detailed.csv"
                .to_string(),
            instrument_csv_fallback_url: "https://images.dhan.co/api-data/api-scrip-master.csv"
                .to_string(),
            max_instruments_per_connection: 5000,
            max_websocket_connections: 5,
        },
        InstrumentConfig {
            daily_download_time: "08:55:00".to_string(),
            csv_cache_directory: "/tmp/dlt-test".to_string(),
            csv_cache_filename: "instruments.csv".to_string(),
            csv_download_timeout_secs: 120,
            build_window_start: "08:25:00".to_string(),
            build_window_end: "08:55:00".to_string(),
        },
        Arc::new(RwLock::new(None)),
        Arc::new(RwLock::new(None)),
    )
}

fn test_app() -> axum::Router {
    build_router(test_state(), &[], true)
}

// ---------------------------------------------------------------------------
// Contract: GET /health
// ---------------------------------------------------------------------------

#[tokio::test]
async fn contract_health_returns_200() {
    let app = test_app();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn contract_health_response_schema() {
    let app = test_app();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // Schema contract: must have "status" and "version" fields
    assert!(json.get("status").is_some(), "missing 'status' field");
    assert!(json.get("version").is_some(), "missing 'version' field");
    assert_eq!(json["status"], "ok");
    assert!(
        !json["version"].as_str().unwrap().is_empty(),
        "version must not be empty"
    );
}

#[tokio::test]
async fn contract_health_content_type_json() {
    let app = test_app();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let content_type = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        content_type.contains("application/json"),
        "content-type should be JSON, got: {content_type}"
    );
}

// ---------------------------------------------------------------------------
// Contract: unknown routes return 404
// ---------------------------------------------------------------------------

#[tokio::test]
async fn contract_unknown_route_returns_404() {
    let app = test_app();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Contract: Dhan REST API types serialization
// ---------------------------------------------------------------------------

#[test]
fn contract_place_order_request_camel_case() {
    use dhan_live_trader_trading::oms::types::DhanPlaceOrderRequest;

    let req = DhanPlaceOrderRequest {
        dhan_client_id: "100".to_owned(),
        transaction_type: "BUY".to_owned(),
        exchange_segment: "NSE_FNO".to_owned(),
        product_type: "INTRADAY".to_owned(),
        order_type: "LIMIT".to_owned(),
        validity: "DAY".to_owned(),
        security_id: "52432".to_owned(),
        quantity: 50,
        price: 245.50,
        trigger_price: 0.0,
        disclosed_quantity: 0,
        after_market_order: false,
        correlation_id: "uuid-1".to_owned(),
    };

    let json = serde_json::to_value(&req).unwrap();

    // Contract: Dhan REST API requires camelCase keys
    assert!(
        json.get("dhanClientId").is_some(),
        "must use camelCase: dhanClientId"
    );
    assert!(
        json.get("transactionType").is_some(),
        "must use camelCase: transactionType"
    );
    assert!(
        json.get("exchangeSegment").is_some(),
        "must use camelCase: exchangeSegment"
    );
    assert!(
        json.get("productType").is_some(),
        "must use camelCase: productType"
    );
    assert!(
        json.get("orderType").is_some(),
        "must use camelCase: orderType"
    );
    assert!(
        json.get("securityId").is_some(),
        "must use camelCase: securityId"
    );
    assert!(
        json.get("correlationId").is_some(),
        "must use camelCase: correlationId"
    );
    assert!(
        json.get("afterMarketOrder").is_some(),
        "must use camelCase: afterMarketOrder"
    );
    assert!(
        json.get("disclosedQuantity").is_some(),
        "must use camelCase: disclosedQuantity"
    );
    assert!(
        json.get("triggerPrice").is_some(),
        "must use camelCase: triggerPrice"
    );

    // Must NOT contain snake_case keys
    assert!(
        json.get("dhan_client_id").is_none(),
        "snake_case key leaked: dhan_client_id"
    );
    assert!(
        json.get("transaction_type").is_none(),
        "snake_case key leaked: transaction_type"
    );
}

#[test]
fn contract_place_order_response_deserialization() {
    use dhan_live_trader_trading::oms::types::DhanPlaceOrderResponse;

    // Minimal Dhan response
    let json_str = r#"{"orderId":"ORD-123","orderStatus":"TRANSIT","correlationId":"uuid-1"}"#;
    let resp: DhanPlaceOrderResponse = serde_json::from_str(json_str).unwrap();
    assert_eq!(resp.order_id, "ORD-123");
    assert_eq!(resp.order_status, "TRANSIT");
    assert_eq!(resp.correlation_id, "uuid-1");
}

#[test]
fn contract_order_response_handles_missing_optional_fields() {
    use dhan_live_trader_trading::oms::types::DhanOrderResponse;

    // Minimal response — most fields missing (serde defaults)
    let json_str = r#"{"orderId":"999","orderStatus":"TRADED"}"#;
    let resp: DhanOrderResponse = serde_json::from_str(json_str).unwrap();
    assert_eq!(resp.order_id, "999");
    assert_eq!(resp.order_status, "TRADED");
    assert_eq!(resp.quantity, 0);
    assert_eq!(resp.price, 0.0);
    assert!(resp.correlation_id.is_empty());
    assert!(resp.trading_symbol.is_empty());
}

#[test]
fn contract_position_response_deserialization() {
    use dhan_live_trader_trading::oms::types::DhanPositionResponse;

    let json_str = r#"{
        "dhanClientId": "100",
        "securityId": "52432",
        "exchangeSegment": "NSE_FNO",
        "productType": "INTRADAY",
        "positionType": "LONG",
        "buyQty": 50,
        "sellQty": 0,
        "netQty": 50,
        "buyAvg": 245.50,
        "sellAvg": 0.0,
        "realizedProfit": 0.0,
        "unrealizedProfit": 125.50
    }"#;

    let resp: DhanPositionResponse = serde_json::from_str(json_str).unwrap();
    assert_eq!(resp.security_id, "52432");
    assert_eq!(resp.buy_qty, 50);
    assert_eq!(resp.net_qty, 50);
    assert!((resp.buy_avg - 245.50).abs() < 0.01);
}
