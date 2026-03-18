//! Builds JSON subscription messages for the Dhan WebSocket V2 protocol.
//!
//! Dhan limits each subscription message to 100 instruments. This module
//! splits a list of instruments into batched JSON messages ready to send.

use dhan_live_trader_common::constants::SUBSCRIPTION_BATCH_SIZE;
use dhan_live_trader_common::types::{ExchangeSegment, FeedMode};

use crate::websocket::types::{
    InstrumentSubscription, SubscriptionRequest, TwoHundredDepthSubscriptionRequest,
};

/// Maps a `FeedMode` to the Dhan WebSocket subscribe RequestCode.
///
/// - Ticker → 15
/// - Quote  → 17
/// - Full   → 21
fn feed_mode_to_subscribe_code(mode: FeedMode) -> u8 {
    match mode {
        FeedMode::Ticker => dhan_live_trader_common::constants::FEED_REQUEST_TICKER,
        FeedMode::Quote => dhan_live_trader_common::constants::FEED_REQUEST_QUOTE,
        FeedMode::Full => dhan_live_trader_common::constants::FEED_REQUEST_FULL,
    }
}

/// Maps a `FeedMode` to the Dhan WebSocket unsubscribe RequestCode.
///
/// Per Dhan SDK: unsubscribe_code = subscribe_code + 1.
/// - Ticker → 16
/// - Quote  → 18
/// - Full   → 22
fn feed_mode_to_unsubscribe_code(mode: FeedMode) -> u8 {
    match mode {
        FeedMode::Ticker => dhan_live_trader_common::constants::FEED_UNSUBSCRIBE_TICKER,
        FeedMode::Quote => dhan_live_trader_common::constants::FEED_UNSUBSCRIBE_QUOTE,
        FeedMode::Full => dhan_live_trader_common::constants::FEED_UNSUBSCRIBE_FULL,
    }
}

/// Builds subscription JSON messages from a list of instruments.
///
/// Each message contains at most `batch_size` instruments (Dhan limit: 100).
/// Returns an empty Vec if the input list is empty.
///
/// # Arguments
/// * `instruments` — Full list of instruments to subscribe.
/// * `feed_mode` — Desired feed granularity (Ticker, Quote, Full).
/// * `batch_size` — Max instruments per message (use config value, max 100).
///
/// # Returns
/// Vec of serialized JSON strings, each ready to send over WebSocket.
pub fn build_subscription_messages(
    instruments: &[InstrumentSubscription],
    feed_mode: FeedMode,
    batch_size: usize,
) -> Vec<String> {
    // O(1) EXEMPT: begin — subscription building runs once at connect time, not per tick
    if instruments.is_empty() {
        return Vec::new();
    }

    // Clamp batch size to Dhan's hard limit.
    let effective_batch = batch_size.clamp(1, SUBSCRIPTION_BATCH_SIZE);
    let request_code = feed_mode_to_subscribe_code(feed_mode);

    #[allow(clippy::expect_used)] // APPROVED: SubscriptionRequest is infallible to serialize
    instruments
        .chunks(effective_batch)
        .map(|chunk| {
            let request = SubscriptionRequest {
                request_code,
                instrument_count: chunk.len(),
                instrument_list: chunk.to_vec(),
            };
            serde_json::to_string(&request).expect("SubscriptionRequest serialization cannot fail") // APPROVED: infallible serialize
        })
        .collect()
    // O(1) EXEMPT: end
}

/// Builds unsubscription JSON messages from a list of instruments.
///
/// Per Dhan SDK: unsubscribe_code = subscribe_code + 1.
/// Ticker=16, Quote=18, Full=22.
pub fn build_unsubscription_messages(
    instruments: &[InstrumentSubscription],
    feed_mode: FeedMode,
    batch_size: usize,
) -> Vec<String> {
    // O(1) EXEMPT: begin — unsubscription building runs once at disconnect time
    if instruments.is_empty() {
        return Vec::new();
    }

    let effective_batch = batch_size.clamp(1, SUBSCRIPTION_BATCH_SIZE);
    let unsubscribe_code = feed_mode_to_unsubscribe_code(feed_mode);

    #[allow(clippy::expect_used)] // APPROVED: SubscriptionRequest is infallible to serialize
    instruments
        .chunks(effective_batch)
        .map(|chunk| {
            let request = SubscriptionRequest {
                request_code: unsubscribe_code,
                instrument_count: chunk.len(),
                instrument_list: chunk.to_vec(),
            };
            serde_json::to_string(&request).expect("SubscriptionRequest serialization cannot fail") // APPROVED: infallible serialize
        })
        .collect()
    // O(1) EXEMPT: end
}

/// Builds a disconnect JSON message (RequestCode 12).
///
/// This closes the WebSocket connection gracefully on the server side.
pub fn build_disconnect_message() -> String {
    serde_json::json!({
        "RequestCode": dhan_live_trader_common::constants::FEED_REQUEST_DISCONNECT
    })
    .to_string() // O(1) EXEMPT: disconnect message — once at shutdown
}

/// Builds subscription JSON messages for the 20-level depth WebSocket feed.
///
/// Uses RequestCode 23. The 20-depth feed has a limit of 50 instruments per connection.
/// Each subscription message is batched to at most `batch_size` instruments (Dhan limit: 100).
///
/// # Arguments
/// * `instruments` — List of instruments to subscribe for 20-level depth.
/// * `batch_size` — Max instruments per message (use config value, max 100).
///
/// # Returns
/// Vec of serialized JSON strings, each ready to send over WebSocket.
pub fn build_twenty_depth_subscription_messages(
    instruments: &[InstrumentSubscription],
    batch_size: usize,
) -> Vec<String> {
    // O(1) EXEMPT: begin — subscription building runs once at connect time
    if instruments.is_empty() {
        return Vec::new();
    }

    let effective_batch = batch_size.clamp(1, SUBSCRIPTION_BATCH_SIZE);
    let request_code = dhan_live_trader_common::constants::FEED_REQUEST_TWENTY_DEPTH;

    #[allow(clippy::expect_used)] // APPROVED: SubscriptionRequest is infallible to serialize
    instruments
        .chunks(effective_batch)
        .map(|chunk| {
            let request = SubscriptionRequest {
                request_code,
                instrument_count: chunk.len(),
                instrument_list: chunk.to_vec(),
            };
            serde_json::to_string(&request).expect("SubscriptionRequest serialization cannot fail") // APPROVED: infallible serialize
        })
        .collect()
    // O(1) EXEMPT: end
}

/// Builds unsubscription JSON messages for the 20-level depth WebSocket feed.
///
/// Uses RequestCode 25 (Dhan Annexure: UnsubscribeFullDepth = 25).
pub fn build_twenty_depth_unsubscription_messages(
    instruments: &[InstrumentSubscription],
    batch_size: usize,
) -> Vec<String> {
    // O(1) EXEMPT: begin — unsubscription building runs once at disconnect time
    if instruments.is_empty() {
        return Vec::new();
    }

    let effective_batch = batch_size.clamp(1, SUBSCRIPTION_BATCH_SIZE);
    let unsubscribe_code = dhan_live_trader_common::constants::FEED_UNSUBSCRIBE_TWENTY_DEPTH;

    #[allow(clippy::expect_used)] // APPROVED: SubscriptionRequest is infallible to serialize
    instruments
        .chunks(effective_batch)
        .map(|chunk| {
            let request = SubscriptionRequest {
                request_code: unsubscribe_code,
                instrument_count: chunk.len(),
                instrument_list: chunk.to_vec(),
            };
            serde_json::to_string(&request).expect("SubscriptionRequest serialization cannot fail") // APPROVED: infallible serialize
        })
        .collect()
    // O(1) EXEMPT: end
}

/// Builds a 200-level depth subscription JSON message for a single instrument.
///
/// Uses RequestCode 23. Flat JSON structure (no InstrumentList array).
/// 200-level depth supports only 1 instrument per connection.
///
/// # Arguments
/// * `segment` — Exchange segment (must be NSE_EQ or NSE_FNO — validated).
/// * `security_id` — Dhan security identifier.
///
/// # Returns
/// * `Ok(String)` — Serialized JSON subscription message.
/// * `Err(String)` — If the segment is not NSE (depth only supports NSE).
// TEST-EXEMPT: tested via test_two_hundred_depth_subscription_nse_eq, test_two_hundred_depth_subscription_nse_fno
pub fn build_two_hundred_depth_subscription_message(
    segment: ExchangeSegment,
    security_id: u32,
) -> Result<String, String> {
    // O(1) EXEMPT: begin — subscription building runs once at connect time
    validate_depth_segment(segment)?;

    let request = TwoHundredDepthSubscriptionRequest {
        request_code: dhan_live_trader_common::constants::FEED_REQUEST_TWENTY_DEPTH, // 23 for both 20 and 200 depth
        exchange_segment: segment.as_str().to_string(),
        security_id: security_id.to_string(),
    };

    #[allow(clippy::expect_used)]
    // APPROVED: TwoHundredDepthSubscriptionRequest is infallible to serialize
    Ok(serde_json::to_string(&request)
        .expect("TwoHundredDepthSubscriptionRequest serialization cannot fail"))
    // O(1) EXEMPT: end
}

/// Builds a 200-level depth unsubscription JSON message for a single instrument.
///
/// Uses RequestCode 25.
// TEST-EXEMPT: tested via test_two_hundred_depth_unsubscription_request_code_25
pub fn build_two_hundred_depth_unsubscription_message(
    segment: ExchangeSegment,
    security_id: u32,
) -> Result<String, String> {
    // O(1) EXEMPT: begin — unsubscription building runs once at disconnect time
    validate_depth_segment(segment)?;

    let request = TwoHundredDepthSubscriptionRequest {
        request_code: dhan_live_trader_common::constants::FEED_UNSUBSCRIBE_TWENTY_DEPTH, // 25
        exchange_segment: segment.as_str().to_string(),
        security_id: security_id.to_string(),
    };

    #[allow(clippy::expect_used)]
    // APPROVED: TwoHundredDepthSubscriptionRequest is infallible to serialize
    Ok(serde_json::to_string(&request)
        .expect("TwoHundredDepthSubscriptionRequest serialization cannot fail"))
    // O(1) EXEMPT: end
}

/// Validates that the exchange segment is NSE-only (NSE_EQ or NSE_FNO).
///
/// Full Market Depth (20-level and 200-level) only supports NSE segments.
/// BSE, MCX, Currency are NOT available. Reject at subscription build time.
pub fn validate_depth_segment(segment: ExchangeSegment) -> Result<(), String> {
    match segment {
        ExchangeSegment::NseEquity | ExchangeSegment::NseFno => Ok(()),
        // O(1) EXEMPT: subscription validation runs once at connect time, not per tick
        other => {
            let seg = other.as_str();
            Err([
                "Full Market Depth only supports NSE_EQ and NSE_FNO, got: ",
                seg,
            ]
            .concat())
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test code
mod tests {
    use super::*;
    use dhan_live_trader_common::types::ExchangeSegment;

    fn make_instruments(count: usize) -> Vec<InstrumentSubscription> {
        (0..count)
            .map(|i| InstrumentSubscription::new(ExchangeSegment::NseFno, (i as u32) + 1000))
            .collect()
    }

    #[test]
    fn test_empty_instruments_returns_empty() {
        let messages = build_subscription_messages(&[], FeedMode::Ticker, 100);
        assert!(messages.is_empty());
    }

    #[test]
    fn test_single_instrument_single_message() {
        let instruments = make_instruments(1);
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, 100);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("\"RequestCode\":15"));
        assert!(messages[0].contains("\"InstrumentCount\":1"));
    }

    #[test]
    fn test_exact_batch_boundary() {
        let instruments = make_instruments(100);
        let messages = build_subscription_messages(&instruments, FeedMode::Quote, 100);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("\"InstrumentCount\":100"));
    }

    #[test]
    fn test_batch_splits_at_boundary() {
        let instruments = make_instruments(101);
        let messages = build_subscription_messages(&instruments, FeedMode::Full, 100);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("\"InstrumentCount\":100"));
        assert!(messages[1].contains("\"InstrumentCount\":1"));
    }

    #[test]
    fn test_multiple_batches() {
        let instruments = make_instruments(250);
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, 100);
        assert_eq!(messages.len(), 3);
    }

    #[test]
    fn test_batch_size_clamped_to_max() {
        let instruments = make_instruments(200);
        // Even if caller passes 500, it's clamped to 100
        let messages = build_subscription_messages(&instruments, FeedMode::Full, 500);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn test_batch_size_zero_clamped_to_one() {
        let instruments = make_instruments(3);
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, 0);
        assert_eq!(messages.len(), 3);
    }

    #[test]
    fn test_feed_mode_ticker_request_code_15() {
        let instruments = make_instruments(1);
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, 100);
        assert!(messages[0].contains("\"RequestCode\":15"));
    }

    #[test]
    fn test_feed_mode_quote_request_code_17() {
        let instruments = make_instruments(1);
        let messages = build_subscription_messages(&instruments, FeedMode::Quote, 100);
        assert!(messages[0].contains("\"RequestCode\":17"));
    }

    #[test]
    fn test_feed_mode_full_request_code_21() {
        let instruments = make_instruments(1);
        let messages = build_subscription_messages(&instruments, FeedMode::Full, 100);
        assert!(messages[0].contains("\"RequestCode\":21"));
    }

    #[test]
    fn test_security_id_is_string_not_number() {
        let instruments = vec![InstrumentSubscription::new(ExchangeSegment::IdxI, 13)];
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, 100);
        assert!(messages[0].contains("\"SecurityId\":\"13\""));
    }

    #[test]
    fn test_unsubscription_ticker_uses_request_code_16() {
        let instruments = make_instruments(5);
        let messages = build_unsubscription_messages(&instruments, FeedMode::Ticker, 100);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("\"RequestCode\":16"));
    }

    #[test]
    fn test_unsubscription_quote_uses_request_code_18() {
        let instruments = make_instruments(3);
        let messages = build_unsubscription_messages(&instruments, FeedMode::Quote, 100);
        assert!(messages[0].contains("\"RequestCode\":18"));
    }

    #[test]
    fn test_unsubscription_full_uses_request_code_22() {
        let instruments = make_instruments(2);
        let messages = build_unsubscription_messages(&instruments, FeedMode::Full, 100);
        assert!(messages[0].contains("\"RequestCode\":22"));
    }

    #[test]
    fn test_unsubscription_empty_instruments() {
        let messages = build_unsubscription_messages(&[], FeedMode::Ticker, 100);
        assert!(messages.is_empty());
    }

    #[test]
    fn test_unsubscription_batches_correctly() {
        let instruments = make_instruments(150);
        let messages = build_unsubscription_messages(&instruments, FeedMode::Full, 100);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn test_disconnect_message_uses_request_code_12() {
        let msg = build_disconnect_message();
        assert!(msg.contains("\"RequestCode\":12"));
    }

    #[test]
    fn test_batch_size_one_creates_one_per_message() {
        let instruments = make_instruments(5);
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, 1);
        assert_eq!(messages.len(), 5);
        for msg in &messages {
            assert!(msg.contains("\"InstrumentCount\":1"));
        }
    }

    #[test]
    fn test_batch_size_exact_100_single_batch() {
        let instruments = make_instruments(100);
        // batch_size = 100 = SUBSCRIPTION_BATCH_SIZE
        let messages = build_subscription_messages(&instruments, FeedMode::Full, 100);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("\"InstrumentCount\":100"));
    }

    #[test]
    fn test_unsubscription_batch_size_clamped_above_100() {
        let instruments = make_instruments(200);
        // batch_size = 999 clamped to 100
        let messages = build_unsubscription_messages(&instruments, FeedMode::Ticker, 999);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("\"RequestCode\":16"));
    }

    #[test]
    fn test_mixed_exchange_segments_in_batch() {
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1000),
            InstrumentSubscription::new(ExchangeSegment::IdxI, 13),
            InstrumentSubscription::new(ExchangeSegment::BseFno, 2000),
            InstrumentSubscription::new(ExchangeSegment::NseEquity, 2885),
        ];
        let messages = build_subscription_messages(&instruments, FeedMode::Quote, 100);
        assert_eq!(messages.len(), 1);
        let json = &messages[0];
        assert!(json.contains("NSE_FNO"));
        assert!(json.contains("IDX_I"));
        assert!(json.contains("BSE_FNO"));
        assert!(json.contains("NSE_EQ"));
    }

    #[test]
    fn test_large_security_id_u32_max_as_string() {
        let instruments = vec![InstrumentSubscription::new(
            ExchangeSegment::NseFno,
            u32::MAX,
        )];
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, 100);
        assert!(messages[0].contains(&format!("\"SecurityId\":\"{}\"", u32::MAX)));
    }

    #[test]
    fn test_all_exchange_segments_serialize_correctly() {
        let segments = [
            (ExchangeSegment::IdxI, "IDX_I"),
            (ExchangeSegment::NseEquity, "NSE_EQ"),
            (ExchangeSegment::NseFno, "NSE_FNO"),
            (ExchangeSegment::BseEquity, "BSE_EQ"),
            (ExchangeSegment::BseFno, "BSE_FNO"),
        ];
        for (segment, expected_str) in &segments {
            let instruments = vec![InstrumentSubscription::new(*segment, 100)];
            let messages = build_subscription_messages(&instruments, FeedMode::Full, 100);
            assert!(
                messages[0].contains(expected_str),
                "Expected {expected_str} in JSON for segment {segment:?}",
            );
        }
    }

    #[test]
    fn test_valid_json_parse() {
        let instruments = make_instruments(3);
        let messages = build_subscription_messages(&instruments, FeedMode::Full, 100);
        // Must be valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&messages[0]).unwrap();
        assert_eq!(parsed["RequestCode"], 21);
        assert_eq!(parsed["InstrumentCount"], 3);
        assert_eq!(parsed["InstrumentList"].as_array().unwrap().len(), 3);
    }

    // --- 20-depth subscription ---

    #[test]
    fn test_twenty_depth_subscription_request_code_23() {
        let instruments = make_instruments(1);
        let messages = build_twenty_depth_subscription_messages(&instruments, 100);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("\"RequestCode\":23"));
    }

    #[test]
    fn test_twenty_depth_subscription_empty() {
        let messages = build_twenty_depth_subscription_messages(&[], 100);
        assert!(messages.is_empty());
    }

    #[test]
    fn test_twenty_depth_subscription_batching() {
        let instruments = make_instruments(150);
        let messages = build_twenty_depth_subscription_messages(&instruments, 100);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("\"InstrumentCount\":100"));
        assert!(messages[1].contains("\"InstrumentCount\":50"));
    }

    #[test]
    fn test_twenty_depth_unsubscription_request_code_25() {
        let instruments = make_instruments(3);
        let messages = build_twenty_depth_unsubscription_messages(&instruments, 100);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("\"RequestCode\":25"));
    }

    #[test]
    fn test_twenty_depth_unsubscription_empty() {
        let messages = build_twenty_depth_unsubscription_messages(&[], 100);
        assert!(messages.is_empty());
    }

    #[test]
    fn test_twenty_depth_subscription_valid_json() {
        let instruments = make_instruments(5);
        let messages = build_twenty_depth_subscription_messages(&instruments, 100);
        let parsed: serde_json::Value = serde_json::from_str(&messages[0]).unwrap();
        assert_eq!(parsed["RequestCode"], 23);
        assert_eq!(parsed["InstrumentCount"], 5);
        assert_eq!(parsed["InstrumentList"].as_array().unwrap().len(), 5);
    }

    // --- 200-depth subscription ---

    #[test]
    fn test_two_hundred_depth_subscription_nse_eq() {
        let msg =
            build_two_hundred_depth_subscription_message(ExchangeSegment::NseEquity, 1333).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["RequestCode"], 23);
        assert_eq!(parsed["ExchangeSegment"], "NSE_EQ");
        assert_eq!(parsed["SecurityId"], "1333");
        // Must NOT have InstrumentList or InstrumentCount (flat structure)
        assert!(parsed.get("InstrumentList").is_none());
        assert!(parsed.get("InstrumentCount").is_none());
    }

    #[test]
    fn test_two_hundred_depth_subscription_nse_fno() {
        let msg =
            build_two_hundred_depth_subscription_message(ExchangeSegment::NseFno, 52432).unwrap();
        assert!(msg.contains("\"ExchangeSegment\":\"NSE_FNO\""));
        assert!(msg.contains("\"SecurityId\":\"52432\""));
    }

    #[test]
    fn test_two_hundred_depth_subscription_rejects_bse() {
        let result = build_two_hundred_depth_subscription_message(ExchangeSegment::BseEquity, 1000);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("NSE_EQ and NSE_FNO"));
    }

    #[test]
    fn test_two_hundred_depth_subscription_rejects_idx() {
        let result = build_two_hundred_depth_subscription_message(ExchangeSegment::IdxI, 13);
        assert!(result.is_err());
    }

    #[test]
    fn test_two_hundred_depth_unsubscription_request_code_25() {
        let msg = build_two_hundred_depth_unsubscription_message(ExchangeSegment::NseEquity, 2885)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["RequestCode"], 25);
        assert_eq!(parsed["SecurityId"], "2885");
    }

    #[test]
    fn test_two_hundred_depth_unsubscription_rejects_non_nse() {
        let result = build_two_hundred_depth_unsubscription_message(ExchangeSegment::BseFno, 99999);
        assert!(result.is_err());
    }

    // --- NSE-only validation ---

    #[test]
    fn test_validate_depth_segment_nse_eq_ok() {
        assert!(validate_depth_segment(ExchangeSegment::NseEquity).is_ok());
    }

    #[test]
    fn test_validate_depth_segment_nse_fno_ok() {
        assert!(validate_depth_segment(ExchangeSegment::NseFno).is_ok());
    }

    #[test]
    fn test_validate_depth_segment_bse_eq_rejected() {
        assert!(validate_depth_segment(ExchangeSegment::BseEquity).is_err());
    }

    #[test]
    fn test_validate_depth_segment_bse_fno_rejected() {
        assert!(validate_depth_segment(ExchangeSegment::BseFno).is_err());
    }

    #[test]
    fn test_validate_depth_segment_idx_rejected() {
        assert!(validate_depth_segment(ExchangeSegment::IdxI).is_err());
    }
}
