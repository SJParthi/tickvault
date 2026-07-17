//! Native Rust protobuf decoders for the Groww ORDER/POSITION push payloads
//! (order-push Stage B, 2026-07-16). The proto3 PRIMITIVE decoders
//! (bounds-checked varint / fixed64 / len-delimited cursor +
//! [`ProtoDecodeError`]) are restored byte-faithful from the retired
//! live-feed decoder (`dd7eaa5e^:crates/core/src/feed/groww/proto.rs`); the
//! tick DTOs (`GrowwLivePrice` / `GrowwIndexValue`) are DROPPED and replaced
//! by the order/position DTOs below.
//!
//! ## VERIFIED wire schema (derived from the official growwapi-1.5.0 wheel —
//! NOT guessed)
//!
//! Field numbers + types were extracted 2026-07-16 from the wheel's
//! serialized descriptors (`pip download growwapi==1.5.0`):
//! - `growwapi/groww/proto/stock_orders_socket_response_pb2.py`
//!   (source `StockOrdersSocketResponse.proto`, no package)
//! - `growwapi/groww/proto/position_socket_pb2.py`
//!   (source `PositionSocket.proto`, package `groww.protobuf.fno.dto.fnoData`)
//!
//! Full field map + enum tables with the descriptor evidence:
//! `docs/groww-ref/18-order-position-proto.md`.
//!
//! `OrderDetailsBroadCastDto` (the ORDER-subject MSG body):
//! | # | field | wire |
//! |---|-------|------|
//! | 1 | stageAndTimeStamp | repeated message → [`StageAndTimeStamp`] |
//! | 2 | orderDetailUpdateDto | message → [`OrderDetailUpdate`] |
//!
//! `OrderDetailUpdateDto` — **all prices are `int64` PAISE (varint)**, never
//! floats: 1 qty int32 · 2 price int64 · 3 triggerPrice int64 ·
//! 4 filledQty int32 · 5 remainingQty int32 · 6 avgFillPrice int64 ·
//! 7 growwOrderId string · 8 exchangeOrderId string · 9 orderStatus enum ·
//! 10 duration enum · 11 exchange enum · 12 segment enum · 13 product enum ·
//! 14 orderType enum · 15 buySell enum · 16 remark string ·
//! 22 contractId string · 23 guiOrderId string (17–21 unassigned in the
//! descriptor — skipped as unknown if they ever appear).
//!
//! `PositionDetailProto` (the POSITION-subject MSG body):
//! 1 symbolData → [`SymbolInfo`] · 2 positionInfo → [`PositionInfo`].
//! `SymbolInfoProto`: 1 trTimeStamp int64 · 2 searchId string ·
//! 3 stocksProduct enum · 4 contractId string · 5 equityType enum ·
//! 6 displayName string · 7 underlyingId string · 8 nseMarketLot int64 ·
//! 9 bseMarketLot int64 · 10 underlyingAssetType enum · 11 freezeQty int64 ·
//! 12 exchange enum. `PositionInfoProto`: 1 symbolIsin string ·
//! 2 BSE message · 3 NSE message; `BseProto`/`NseProto`: 1 creditQty double ·
//! 2 creditPrice double · 3 debitQty double · 4 debitPrice double —
//! **position qty/price fields are IEEE-754 doubles per the descriptor**
//! (only the ORDER prices are int64 paise).
//!
//! Enum divergence note: the POSITION file's `StockExchange` carries
//! `GLOBAL = 5, US = 6` (the ORDER file has `US = 5`, no GLOBAL) — hence the
//! separate [`position_exchange_name`] mapper; its `StocksProduct` adds
//! `_unknown = 7` beyond the shared `CNC..MTF = 0..6`.
//!
//! ## Guarantees (operator O(1) / worst-case demand)
//!
//! - **No panic on any input:** every read is bounds-checked and returns a
//!   typed [`ProtoDecodeError`] (a `Copy` enum — no heap even on the error
//!   path). Truncated/garbage/over-long-varint inputs all return `Err` or
//!   skip cleanly.
//! - **Tolerant / forward-compatible:** unknown field numbers, unexpected
//!   wire types on known fields, and non-UTF-8 string bytes (lossy-decoded)
//!   are all absorbed, so a future Groww schema addition does not break
//!   decoding. Enum values are kept RAW (open-set — annexure rule 15 /
//!   GROWW-ORD-07 discipline); the `*_name` mappers are total and return
//!   `None` for anything outside the descriptor's vocabulary.
//! - **Cold path:** order/position pushes are a handful of frames per order
//!   lifecycle — the owned `String` fields are intentional and nowhere near
//!   the tick hot path.

/// Field numbers in `OrderDetailsBroadCastDto`.
const OD_STAGE_AND_TIMESTAMP: u64 = 1;
const OD_ORDER_DETAIL_UPDATE: u64 = 2;

/// Field numbers in `StageAndTimeStamp`.
const ST_TIMESTAMP_FROM_MIDNIGHT: u64 = 1;
const ST_STAGE_NAME: u64 = 2;

/// Field numbers in `OrderDetailUpdateDto`.
const OU_QTY: u64 = 1;
const OU_PRICE: u64 = 2;
const OU_TRIGGER_PRICE: u64 = 3;
const OU_FILLED_QTY: u64 = 4;
const OU_REMAINING_QTY: u64 = 5;
const OU_AVG_FILL_PRICE: u64 = 6;
const OU_GROWW_ORDER_ID: u64 = 7;
const OU_EXCHANGE_ORDER_ID: u64 = 8;
const OU_ORDER_STATUS: u64 = 9;
const OU_DURATION: u64 = 10;
const OU_EXCHANGE: u64 = 11;
const OU_SEGMENT: u64 = 12;
const OU_PRODUCT: u64 = 13;
const OU_ORDER_TYPE: u64 = 14;
const OU_BUY_SELL: u64 = 15;
const OU_REMARK: u64 = 16;
const OU_CONTRACT_ID: u64 = 22;
const OU_GUI_ORDER_ID: u64 = 23;

/// Field numbers in `PositionDetailProto`.
const PD_SYMBOL_DATA: u64 = 1;
const PD_POSITION_INFO: u64 = 2;

/// Field numbers in `SymbolInfoProto`.
const SI_TR_TIMESTAMP: u64 = 1;
const SI_SEARCH_ID: u64 = 2;
const SI_STOCKS_PRODUCT: u64 = 3;
const SI_CONTRACT_ID: u64 = 4;
const SI_EQUITY_TYPE: u64 = 5;
const SI_DISPLAY_NAME: u64 = 6;
const SI_UNDERLYING_ID: u64 = 7;
const SI_NSE_MARKET_LOT: u64 = 8;
const SI_BSE_MARKET_LOT: u64 = 9;
const SI_UNDERLYING_ASSET_TYPE: u64 = 10;
const SI_FREEZE_QTY: u64 = 11;
const SI_EXCHANGE: u64 = 12;

/// Field numbers in `PositionInfoProto`.
const PI_SYMBOL_ISIN: u64 = 1;
const PI_BSE: u64 = 2;
const PI_NSE: u64 = 3;

/// Field numbers in `BseProto` / `NseProto` (identical shape).
const XP_CREDIT_QTY: u64 = 1;
const XP_CREDIT_PRICE: u64 = 2;
const XP_DEBIT_QTY: u64 = 3;
const XP_DEBIT_PRICE: u64 = 4;

/// Protobuf wire types we handle.
const WIRE_VARINT: u8 = 0;
const WIRE_FIXED64: u8 = 1;
const WIRE_LEN_DELIM: u8 = 2;
const WIRE_FIXED32: u8 = 5;

/// A decode failure. `Copy` (no heap) so the error path is also zero-alloc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtoDecodeError {
    /// Ran off the end of the buffer mid-field.
    Truncated,
    /// A varint exceeded 10 bytes (malformed / not a valid u64 LEB128).
    VarintOverflow,
    /// A wire type we do not recognise (3/4 group-start/end, or 6/7).
    UnknownWireType(u8),
}

impl core::fmt::Display for ProtoDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => write!(f, "groww proto decode: truncated input"),
            Self::VarintOverflow => write!(f, "groww proto decode: varint overflow (>10 bytes)"),
            Self::UnknownWireType(w) => {
                write!(f, "groww proto decode: unknown wire type {w}")
            }
        }
    }
}

impl std::error::Error for ProtoDecodeError {}

/// One order-lifecycle stage marker (`StageAndTimeStamp`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StageAndTimeStamp {
    /// `timeStampFromMidNight` (field 1, double) — milliseconds-from-midnight
    /// shaped value as the wire delivers it; semantics UNVERIFIED-LIVE.
    pub time_stamp_from_midnight: f64,
    /// `stageName` (field 2, string).
    pub stage_name: String,
}

/// The decoded `OrderDetailUpdateDto` — one order/trade update. Absent fields
/// default per proto3 semantics (0 / empty). Enum fields are kept RAW
/// (open-set); map via the `*_name` helpers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OrderDetailUpdate {
    /// `qty` (field 1, int32).
    pub qty: i32,
    /// `price` (field 2, int64) — **PAISE**.
    pub price_paise: i64,
    /// `triggerPrice` (field 3, int64) — **PAISE**.
    pub trigger_price_paise: i64,
    /// `filledQty` (field 4, int32).
    pub filled_qty: i32,
    /// `remainingQty` (field 5, int32).
    pub remaining_qty: i32,
    /// `avgFillPrice` (field 6, int64) — **PAISE**.
    pub avg_fill_price_paise: i64,
    /// `growwOrderId` (field 7, string).
    pub groww_order_id: String,
    /// `exchangeOrderId` (field 8, string).
    pub exchange_order_id: String,
    /// `orderStatus` (field 9, enum `StocksOrderStatus.Enum`) — RAW value;
    /// see [`order_status_name`].
    pub order_status: i32,
    /// `duration` (field 10, enum `StocksOrderDuration.Enum`) — RAW value.
    pub duration: i32,
    /// `exchange` (field 11, enum `StockExchange.Enum`, ORDER-file variant)
    /// — RAW value.
    pub exchange: i32,
    /// `segment` (field 12, enum `StockSegment.Enum`) — RAW value.
    pub segment: i32,
    /// `product` (field 13, enum `StocksProduct.Enum`) — RAW value.
    pub product: i32,
    /// `orderType` (field 14, enum `StocksOrderType.Enum`) — RAW value.
    pub order_type: i32,
    /// `buySell` (field 15, enum `StocksBuySell.Enum`) — RAW value.
    pub buy_sell: i32,
    /// `remark` (field 16, string).
    pub remark: String,
    /// `contractId` (field 22, string — fields 17–21 are unassigned).
    pub contract_id: String,
    /// `guiOrderId` (field 23, string).
    pub gui_order_id: String,
}

/// The decoded ORDER-subject MSG body (`OrderDetailsBroadCastDto`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OrderDetailsBroadCastDto {
    /// `stageAndTimeStamp` (field 1, repeated).
    pub stage_and_time_stamp: Vec<StageAndTimeStamp>,
    /// `orderDetailUpdateDto` (field 2). `None` when the frame carried no
    /// order-detail submessage.
    pub order_detail: Option<OrderDetailUpdate>,
}

/// Per-exchange position leg (`BseProto` / `NseProto` — identical shape).
/// All four fields are IEEE-754 doubles per the descriptor.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ExchangePosition {
    /// `creditQty` (field 1, double).
    pub credit_qty: f64,
    /// `creditPrice` (field 2, double).
    pub credit_price: f64,
    /// `debitQty` (field 3, double).
    pub debit_qty: f64,
    /// `debitPrice` (field 4, double).
    pub debit_price: f64,
}

/// Decoded `SymbolInfoProto` — the position's instrument identity block.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SymbolInfo {
    /// `trTimeStamp` (field 1, int64).
    pub tr_time_stamp: i64,
    /// `searchId` (field 2, string).
    pub search_id: String,
    /// `stocksProduct` (field 3, enum, POSITION-file variant with
    /// `_unknown = 7`) — RAW value.
    pub stocks_product: i32,
    /// `contractId` (field 4, string).
    pub contract_id: String,
    /// `equityType` (field 5, enum) — RAW value; see [`equity_type_name`].
    pub equity_type: i32,
    /// `displayName` (field 6, string).
    pub display_name: String,
    /// `underlyingId` (field 7, string).
    pub underlying_id: String,
    /// `nseMarketLot` (field 8, int64).
    pub nse_market_lot: i64,
    /// `bseMarketLot` (field 9, int64).
    pub bse_market_lot: i64,
    /// `underlyingAssetType` (field 10, enum `EquityAsset`) — RAW value.
    pub underlying_asset_type: i32,
    /// `freezeQty` (field 11, int64).
    pub freeze_qty: i64,
    /// `exchange` (field 12, enum, POSITION-file variant) — RAW value; see
    /// [`position_exchange_name`].
    pub exchange: i32,
}

/// Decoded `PositionInfoProto` — the per-exchange position legs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PositionInfo {
    /// `symbolIsin` (field 1, string).
    pub symbol_isin: String,
    /// `BSE` (field 2). `None` when absent.
    pub bse: Option<ExchangePosition>,
    /// `NSE` (field 3). `None` when absent.
    pub nse: Option<ExchangePosition>,
}

/// The decoded POSITION-subject MSG body (`PositionDetailProto`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PositionDetailProto {
    /// `symbolData` (field 1). `None` when absent.
    pub symbol_data: Option<SymbolInfo>,
    /// `positionInfo` (field 2). `None` when absent.
    pub position_info: Option<PositionInfo>,
}

/// Zero-copy protobuf cursor over a byte slice. No allocation; bounds-checked.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    #[inline]
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    #[inline]
    const fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// LEB128 varint, max 10 bytes (a u64). Returns `VarintOverflow` past 10.
    #[inline]
    fn read_varint(&mut self) -> Result<u64, ProtoDecodeError> {
        let mut result: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            if shift >= 64 {
                return Err(ProtoDecodeError::VarintOverflow);
            }
            let byte = *self.buf.get(self.pos).ok_or(ProtoDecodeError::Truncated)?;
            self.pos += 1;
            result |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }

    /// Tag = `(field_number << 3) | wire_type`.
    #[inline]
    fn read_tag(&mut self) -> Result<(u64, u8), ProtoDecodeError> {
        let tag = self.read_varint()?;
        Ok((tag >> 3, (tag & 0x07) as u8))
    }

    /// 8 little-endian bytes → `f64`.
    #[inline]
    fn read_f64(&mut self) -> Result<f64, ProtoDecodeError> {
        let end = self.pos.checked_add(8).ok_or(ProtoDecodeError::Truncated)?;
        let bytes = self
            .buf
            .get(self.pos..end)
            .ok_or(ProtoDecodeError::Truncated)?;
        let arr: [u8; 8] = bytes.try_into().map_err(|_| ProtoDecodeError::Truncated)?;
        self.pos = end;
        Ok(f64::from_le_bytes(arr))
    }

    /// A length-delimited field → its inner byte slice (no copy).
    #[inline]
    fn read_len_delimited(&mut self) -> Result<&'a [u8], ProtoDecodeError> {
        let len = self.read_varint()? as usize;
        let end = self
            .pos
            .checked_add(len)
            .ok_or(ProtoDecodeError::Truncated)?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or(ProtoDecodeError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    /// A length-delimited string field, lossy-decoded (tolerant — non-UTF-8
    /// bytes become U+FFFD, never an error).
    #[inline]
    fn read_string_lossy(&mut self) -> Result<String, ProtoDecodeError> {
        let bytes = self.read_len_delimited()?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    /// A varint field decoded as proto3 `int32`/enum (the wire sign-extends
    /// negatives to 64 bits; the value is the low 32 bits).
    #[inline]
    fn read_i32(&mut self) -> Result<i32, ProtoDecodeError> {
        Ok(self.read_varint()? as i32)
    }

    /// A varint field decoded as proto3 `int64`.
    #[inline]
    fn read_i64(&mut self) -> Result<i64, ProtoDecodeError> {
        Ok(self.read_varint()? as i64)
    }

    /// Skip a field of the given wire type (forward-compat for unknown fields).
    #[inline]
    fn skip(&mut self, wire: u8) -> Result<(), ProtoDecodeError> {
        match wire {
            WIRE_VARINT => {
                self.read_varint()?;
                Ok(())
            }
            WIRE_FIXED64 => {
                let end = self.pos.checked_add(8).ok_or(ProtoDecodeError::Truncated)?;
                if end > self.buf.len() {
                    return Err(ProtoDecodeError::Truncated);
                }
                self.pos = end;
                Ok(())
            }
            WIRE_LEN_DELIM => {
                self.read_len_delimited()?;
                Ok(())
            }
            WIRE_FIXED32 => {
                let end = self.pos.checked_add(4).ok_or(ProtoDecodeError::Truncated)?;
                if end > self.buf.len() {
                    return Err(ProtoDecodeError::Truncated);
                }
                self.pos = end;
                Ok(())
            }
            other => Err(ProtoDecodeError::UnknownWireType(other)),
        }
    }
}

/// Decode a `StageAndTimeStamp` submessage body.
fn decode_stage_and_timestamp(buf: &[u8]) -> Result<StageAndTimeStamp, ProtoDecodeError> {
    let mut r = Reader::new(buf);
    let mut out = StageAndTimeStamp::default();
    while !r.at_end() {
        let (field, wire) = r.read_tag()?;
        match (field, wire) {
            (ST_TIMESTAMP_FROM_MIDNIGHT, WIRE_FIXED64) => {
                out.time_stamp_from_midnight = r.read_f64()?;
            }
            (ST_STAGE_NAME, WIRE_LEN_DELIM) => out.stage_name = r.read_string_lossy()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(out)
}

/// Decode an `OrderDetailUpdateDto` submessage body.
fn decode_order_detail_update(buf: &[u8]) -> Result<OrderDetailUpdate, ProtoDecodeError> {
    let mut r = Reader::new(buf);
    let mut out = OrderDetailUpdate::default();
    while !r.at_end() {
        let (field, wire) = r.read_tag()?;
        match (field, wire) {
            (OU_QTY, WIRE_VARINT) => out.qty = r.read_i32()?,
            (OU_PRICE, WIRE_VARINT) => out.price_paise = r.read_i64()?,
            (OU_TRIGGER_PRICE, WIRE_VARINT) => out.trigger_price_paise = r.read_i64()?,
            (OU_FILLED_QTY, WIRE_VARINT) => out.filled_qty = r.read_i32()?,
            (OU_REMAINING_QTY, WIRE_VARINT) => out.remaining_qty = r.read_i32()?,
            (OU_AVG_FILL_PRICE, WIRE_VARINT) => out.avg_fill_price_paise = r.read_i64()?,
            (OU_GROWW_ORDER_ID, WIRE_LEN_DELIM) => out.groww_order_id = r.read_string_lossy()?,
            (OU_EXCHANGE_ORDER_ID, WIRE_LEN_DELIM) => {
                out.exchange_order_id = r.read_string_lossy()?;
            }
            (OU_ORDER_STATUS, WIRE_VARINT) => out.order_status = r.read_i32()?,
            (OU_DURATION, WIRE_VARINT) => out.duration = r.read_i32()?,
            (OU_EXCHANGE, WIRE_VARINT) => out.exchange = r.read_i32()?,
            (OU_SEGMENT, WIRE_VARINT) => out.segment = r.read_i32()?,
            (OU_PRODUCT, WIRE_VARINT) => out.product = r.read_i32()?,
            (OU_ORDER_TYPE, WIRE_VARINT) => out.order_type = r.read_i32()?,
            (OU_BUY_SELL, WIRE_VARINT) => out.buy_sell = r.read_i32()?,
            (OU_REMARK, WIRE_LEN_DELIM) => out.remark = r.read_string_lossy()?,
            (OU_CONTRACT_ID, WIRE_LEN_DELIM) => out.contract_id = r.read_string_lossy()?,
            (OU_GUI_ORDER_ID, WIRE_LEN_DELIM) => out.gui_order_id = r.read_string_lossy()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(out)
}

/// Decode an ORDER-subject MSG body into an [`OrderDetailsBroadCastDto`].
/// Unknown fields / unexpected wire types are skipped (forward-compatible).
/// Never panics on any input.
pub fn decode_order_details_broadcast(
    buf: &[u8],
) -> Result<OrderDetailsBroadCastDto, ProtoDecodeError> {
    let mut r = Reader::new(buf);
    let mut out = OrderDetailsBroadCastDto::default();
    while !r.at_end() {
        let (field, wire) = r.read_tag()?;
        match (field, wire) {
            (OD_STAGE_AND_TIMESTAMP, WIRE_LEN_DELIM) => {
                let inner = r.read_len_delimited()?;
                out.stage_and_time_stamp
                    .push(decode_stage_and_timestamp(inner)?);
            }
            (OD_ORDER_DETAIL_UPDATE, WIRE_LEN_DELIM) => {
                let inner = r.read_len_delimited()?;
                out.order_detail = Some(decode_order_detail_update(inner)?);
            }
            _ => r.skip(wire)?,
        }
    }
    Ok(out)
}

/// Decode a `BseProto`/`NseProto` submessage body (all doubles).
fn decode_exchange_position(buf: &[u8]) -> Result<ExchangePosition, ProtoDecodeError> {
    let mut r = Reader::new(buf);
    let mut out = ExchangePosition::default();
    while !r.at_end() {
        let (field, wire) = r.read_tag()?;
        if wire == WIRE_FIXED64 {
            let v = r.read_f64()?;
            match field {
                XP_CREDIT_QTY => out.credit_qty = v,
                XP_CREDIT_PRICE => out.credit_price = v,
                XP_DEBIT_QTY => out.debit_qty = v,
                XP_DEBIT_PRICE => out.debit_price = v,
                _ => {} // unknown field, already consumed the 8 bytes
            }
        } else {
            r.skip(wire)?;
        }
    }
    Ok(out)
}

/// Decode a `SymbolInfoProto` submessage body.
fn decode_symbol_info(buf: &[u8]) -> Result<SymbolInfo, ProtoDecodeError> {
    let mut r = Reader::new(buf);
    let mut out = SymbolInfo::default();
    while !r.at_end() {
        let (field, wire) = r.read_tag()?;
        match (field, wire) {
            (SI_TR_TIMESTAMP, WIRE_VARINT) => out.tr_time_stamp = r.read_i64()?,
            (SI_SEARCH_ID, WIRE_LEN_DELIM) => out.search_id = r.read_string_lossy()?,
            (SI_STOCKS_PRODUCT, WIRE_VARINT) => out.stocks_product = r.read_i32()?,
            (SI_CONTRACT_ID, WIRE_LEN_DELIM) => out.contract_id = r.read_string_lossy()?,
            (SI_EQUITY_TYPE, WIRE_VARINT) => out.equity_type = r.read_i32()?,
            (SI_DISPLAY_NAME, WIRE_LEN_DELIM) => out.display_name = r.read_string_lossy()?,
            (SI_UNDERLYING_ID, WIRE_LEN_DELIM) => out.underlying_id = r.read_string_lossy()?,
            (SI_NSE_MARKET_LOT, WIRE_VARINT) => out.nse_market_lot = r.read_i64()?,
            (SI_BSE_MARKET_LOT, WIRE_VARINT) => out.bse_market_lot = r.read_i64()?,
            (SI_UNDERLYING_ASSET_TYPE, WIRE_VARINT) => {
                out.underlying_asset_type = r.read_i32()?;
            }
            (SI_FREEZE_QTY, WIRE_VARINT) => out.freeze_qty = r.read_i64()?,
            (SI_EXCHANGE, WIRE_VARINT) => out.exchange = r.read_i32()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(out)
}

/// Decode a `PositionInfoProto` submessage body.
fn decode_position_info(buf: &[u8]) -> Result<PositionInfo, ProtoDecodeError> {
    let mut r = Reader::new(buf);
    let mut out = PositionInfo::default();
    while !r.at_end() {
        let (field, wire) = r.read_tag()?;
        match (field, wire) {
            (PI_SYMBOL_ISIN, WIRE_LEN_DELIM) => out.symbol_isin = r.read_string_lossy()?,
            (PI_BSE, WIRE_LEN_DELIM) => {
                let inner = r.read_len_delimited()?;
                out.bse = Some(decode_exchange_position(inner)?);
            }
            (PI_NSE, WIRE_LEN_DELIM) => {
                let inner = r.read_len_delimited()?;
                out.nse = Some(decode_exchange_position(inner)?);
            }
            _ => r.skip(wire)?,
        }
    }
    Ok(out)
}

/// Decode a POSITION-subject MSG body into a [`PositionDetailProto`].
/// Unknown fields / unexpected wire types are skipped (forward-compatible).
/// Never panics on any input.
pub fn decode_position_detail(buf: &[u8]) -> Result<PositionDetailProto, ProtoDecodeError> {
    let mut r = Reader::new(buf);
    let mut out = PositionDetailProto::default();
    while !r.at_end() {
        let (field, wire) = r.read_tag()?;
        match (field, wire) {
            (PD_SYMBOL_DATA, WIRE_LEN_DELIM) => {
                let inner = r.read_len_delimited()?;
                out.symbol_data = Some(decode_symbol_info(inner)?);
            }
            (PD_POSITION_INFO, WIRE_LEN_DELIM) => {
                let inner = r.read_len_delimited()?;
                out.position_info = Some(decode_position_info(inner)?);
            }
            _ => r.skip(wire)?,
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Enum name mappers — total, open-set (annexure rule 15 / GROWW-ORD-07:
// unknown values return `None`, never panic, never guess). Vocabularies
// verified verbatim from the wheel descriptors 2026-07-16.
// ---------------------------------------------------------------------------

/// `StocksOrderStatus.Enum` (ORDER file) name for a raw value.
#[must_use]
pub const fn order_status_name(v: i32) -> Option<&'static str> {
    Some(match v {
        0 => "NEW",
        1 => "ACKED",
        2 => "TRIGGER_PENDING",
        3 => "APPROVED",
        4 => "REJECTED",
        5 => "FAILED",
        6 => "EXECUTED",
        7 => "DELIVERY_AWAITED",
        8 => "CANCELLED",
        9 => "CANCELLATION_REQUESTED",
        10 => "MODIFICATION_REQUESTED",
        11 => "COMPLETED",
        _ => return None,
    })
}

/// `StocksOrderDuration.Enum` (ORDER file) name for a raw value.
#[must_use]
pub const fn duration_name(v: i32) -> Option<&'static str> {
    Some(match v {
        0 => "IOC",
        1 => "DAY",
        2 => "GTD",
        3 => "GTC",
        4 => "EOS",
        _ => return None,
    })
}

/// `StockExchange.Enum` (ORDER file — `US = 5`, no `GLOBAL`) name for a raw
/// value. The POSITION file's divergent variant is [`position_exchange_name`].
#[must_use]
pub const fn exchange_name(v: i32) -> Option<&'static str> {
    Some(match v {
        0 => "BSE",
        1 => "NSE",
        2 => "MCX",
        3 => "MCXSX",
        4 => "NCDEX",
        5 => "US",
        _ => return None,
    })
}

/// `StockSegment.Enum` (ORDER file) name for a raw value.
#[must_use]
pub const fn segment_name(v: i32) -> Option<&'static str> {
    Some(match v {
        0 => "CASH",
        1 => "FNO",
        2 => "CURRENCY",
        3 => "COMMODITY",
        _ => return None,
    })
}

/// `StocksProduct.Enum` (ORDER file; the POSITION file adds `_unknown = 7`,
/// which maps to `None` here — open-set) name for a raw value.
#[must_use]
pub const fn product_name(v: i32) -> Option<&'static str> {
    Some(match v {
        0 => "CNC",
        1 => "MIS",
        2 => "CO",
        3 => "BO",
        4 => "NRML",
        5 => "ARB",
        6 => "MTF",
        _ => return None,
    })
}

/// `StocksOrderType.Enum` (ORDER file) name for a raw value.
#[must_use]
pub const fn order_type_name(v: i32) -> Option<&'static str> {
    Some(match v {
        0 => "MKT",
        1 => "L",
        2 => "SL",
        3 => "SL_M",
        _ => return None,
    })
}

/// `StocksBuySell.Enum` (ORDER file) name for a raw value.
#[must_use]
pub const fn buy_sell_name(v: i32) -> Option<&'static str> {
    Some(match v {
        0 => "B",
        1 => "S",
        _ => return None,
    })
}

/// `EquityType` (POSITION file) name for a raw value.
#[must_use]
pub const fn equity_type_name(v: i32) -> Option<&'static str> {
    Some(match v {
        0 => "STOCKS",
        1 => "FUTURE",
        2 => "OPTION",
        3 => "ETF",
        4 => "INDEX",
        5 => "BONDS",
        _ => return None,
    })
}

/// `StockExchange` (POSITION file — `GLOBAL = 5, US = 6`) name for a raw
/// value. Diverges from the ORDER file's [`exchange_name`] at 5/6.
#[must_use]
pub const fn position_exchange_name(v: i32) -> Option<&'static str> {
    Some(match v {
        0 => "BSE",
        1 => "NSE",
        2 => "MCX",
        3 => "MCXSX",
        4 => "NCDEX",
        5 => "GLOBAL",
        6 => "US",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc_varint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if v == 0 {
                break;
            }
        }
        out
    }

    /// Encode `(field, u64)` as a protobuf varint (wire type 0) entry.
    fn enc_varint_field(field: u64, v: u64) -> Vec<u8> {
        let mut out = enc_varint(field << 3);
        out.extend_from_slice(&enc_varint(v));
        out
    }

    /// Encode a proto3 int32/int64 (negatives sign-extend to a 10-byte varint).
    fn enc_int_field(field: u64, v: i64) -> Vec<u8> {
        enc_varint_field(field, v as u64)
    }

    /// Encode `(field, f64)` as a protobuf wire-type-1 (fixed64) entry.
    fn enc_double(field: u64, v: f64) -> Vec<u8> {
        let mut out = enc_varint((field << 3) | u64::from(WIRE_FIXED64));
        out.extend_from_slice(&v.to_le_bytes());
        out
    }

    /// Encode a string / submessage as a length-delimited field.
    fn enc_len_delim(field: u64, inner: &[u8]) -> Vec<u8> {
        let mut out = enc_varint((field << 3) | u64::from(WIRE_LEN_DELIM));
        out.extend_from_slice(&enc_varint(inner.len() as u64));
        out.extend_from_slice(inner);
        out
    }

    /// A full synthetic OrderDetailUpdateDto body.
    fn synthetic_order_detail() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&enc_int_field(OU_QTY, 75));
        b.extend_from_slice(&enc_int_field(OU_PRICE, 2_847_550)); // ₹28,475.50 in paise
        b.extend_from_slice(&enc_int_field(OU_TRIGGER_PRICE, 2_840_000));
        b.extend_from_slice(&enc_int_field(OU_FILLED_QTY, 25));
        b.extend_from_slice(&enc_int_field(OU_REMAINING_QTY, 50));
        b.extend_from_slice(&enc_int_field(OU_AVG_FILL_PRICE, 2_846_000));
        b.extend_from_slice(&enc_len_delim(OU_GROWW_ORDER_ID, b"GMK42abc"));
        b.extend_from_slice(&enc_len_delim(OU_EXCHANGE_ORDER_ID, b"110066568821"));
        b.extend_from_slice(&enc_varint_field(OU_ORDER_STATUS, 6)); // EXECUTED
        b.extend_from_slice(&enc_varint_field(OU_DURATION, 1)); // DAY
        b.extend_from_slice(&enc_varint_field(OU_EXCHANGE, 1)); // NSE
        b.extend_from_slice(&enc_varint_field(OU_SEGMENT, 1)); // FNO
        b.extend_from_slice(&enc_varint_field(OU_PRODUCT, 1)); // MIS
        b.extend_from_slice(&enc_varint_field(OU_ORDER_TYPE, 1)); // L
        b.extend_from_slice(&enc_varint_field(OU_BUY_SELL, 0)); // B
        b.extend_from_slice(&enc_len_delim(OU_REMARK, b"paper"));
        b.extend_from_slice(&enc_len_delim(OU_CONTRACT_ID, b"NIFTY26JUL28500CE"));
        b.extend_from_slice(&enc_len_delim(OU_GUI_ORDER_ID, b"gui-1"));
        b
    }

    #[test]
    fn test_decode_order_details_broadcast_full_frame() {
        let mut stage = Vec::new();
        stage.extend_from_slice(&enc_double(ST_TIMESTAMP_FROM_MIDNIGHT, 34_567_890.0));
        stage.extend_from_slice(&enc_len_delim(ST_STAGE_NAME, b"EXCHANGE"));

        let mut frame = Vec::new();
        frame.extend_from_slice(&enc_len_delim(OD_STAGE_AND_TIMESTAMP, &stage));
        frame.extend_from_slice(&enc_len_delim(OD_STAGE_AND_TIMESTAMP, &stage));
        frame.extend_from_slice(&enc_len_delim(
            OD_ORDER_DETAIL_UPDATE,
            &synthetic_order_detail(),
        ));

        let dto = decode_order_details_broadcast(&frame).expect("decode");
        assert_eq!(dto.stage_and_time_stamp.len(), 2, "repeated field kept");
        assert_eq!(dto.stage_and_time_stamp[0].stage_name, "EXCHANGE");
        assert_eq!(
            dto.stage_and_time_stamp[0].time_stamp_from_midnight,
            34_567_890.0
        );
        let od = dto.order_detail.expect("order detail present");
        assert_eq!(od.qty, 75);
        assert_eq!(od.price_paise, 2_847_550);
        assert_eq!(od.trigger_price_paise, 2_840_000);
        assert_eq!(od.filled_qty, 25);
        assert_eq!(od.remaining_qty, 50);
        assert_eq!(od.avg_fill_price_paise, 2_846_000);
        assert_eq!(od.groww_order_id, "GMK42abc");
        assert_eq!(od.exchange_order_id, "110066568821");
        assert_eq!(order_status_name(od.order_status), Some("EXECUTED"));
        assert_eq!(duration_name(od.duration), Some("DAY"));
        assert_eq!(exchange_name(od.exchange), Some("NSE"));
        assert_eq!(segment_name(od.segment), Some("FNO"));
        assert_eq!(product_name(od.product), Some("MIS"));
        assert_eq!(order_type_name(od.order_type), Some("L"));
        assert_eq!(buy_sell_name(od.buy_sell), Some("B"));
        assert_eq!(od.remark, "paper");
        assert_eq!(od.contract_id, "NIFTY26JUL28500CE");
        assert_eq!(od.gui_order_id, "gui-1");
    }

    /// Boundary: a NEGATIVE paise price (10-byte sign-extended varint), an
    /// i64::MAX price, and a negative qty all decode exactly — never panic,
    /// never wrap wrong.
    #[test]
    fn test_decode_order_details_broadcast_negative_price_and_i64_max_boundary() {
        let mut b = Vec::new();
        b.extend_from_slice(&enc_int_field(OU_PRICE, -150));
        b.extend_from_slice(&enc_int_field(OU_TRIGGER_PRICE, i64::MAX));
        b.extend_from_slice(&enc_int_field(OU_QTY, -1));
        b.extend_from_slice(&enc_int_field(OU_AVG_FILL_PRICE, 0));
        let frame = enc_len_delim(OD_ORDER_DETAIL_UPDATE, &b);
        let od = decode_order_details_broadcast(&frame)
            .expect("decode")
            .order_detail
            .expect("present");
        assert_eq!(od.price_paise, -150);
        assert_eq!(od.trigger_price_paise, i64::MAX);
        assert_eq!(od.qty, -1);
        assert_eq!(od.avg_fill_price_paise, 0);
    }

    #[test]
    fn test_decode_position_detail_full_frame() {
        let mut sym = Vec::new();
        sym.extend_from_slice(&enc_int_field(SI_TR_TIMESTAMP, 1_784_000_000_123));
        sym.extend_from_slice(&enc_len_delim(SI_SEARCH_ID, b"nifty-26jul-28500-ce"));
        sym.extend_from_slice(&enc_varint_field(SI_STOCKS_PRODUCT, 1)); // MIS
        sym.extend_from_slice(&enc_len_delim(SI_CONTRACT_ID, b"NIFTY26JUL28500CE"));
        sym.extend_from_slice(&enc_varint_field(SI_EQUITY_TYPE, 2)); // OPTION
        sym.extend_from_slice(&enc_len_delim(SI_DISPLAY_NAME, b"NIFTY 28500 CE"));
        sym.extend_from_slice(&enc_len_delim(SI_UNDERLYING_ID, b"nifty"));
        sym.extend_from_slice(&enc_int_field(SI_NSE_MARKET_LOT, 75));
        sym.extend_from_slice(&enc_int_field(SI_BSE_MARKET_LOT, 0));
        sym.extend_from_slice(&enc_varint_field(SI_UNDERLYING_ASSET_TYPE, 1)); // INDICES
        sym.extend_from_slice(&enc_int_field(SI_FREEZE_QTY, 1800));
        sym.extend_from_slice(&enc_varint_field(SI_EXCHANGE, 1)); // NSE

        let mut nse = Vec::new();
        nse.extend_from_slice(&enc_double(XP_CREDIT_QTY, 75.0));
        nse.extend_from_slice(&enc_double(XP_CREDIT_PRICE, 152.35));
        nse.extend_from_slice(&enc_double(XP_DEBIT_QTY, 75.0));
        nse.extend_from_slice(&enc_double(XP_DEBIT_PRICE, 148.10));

        let mut info = Vec::new();
        info.extend_from_slice(&enc_len_delim(PI_SYMBOL_ISIN, b"INE000000000"));
        info.extend_from_slice(&enc_len_delim(PI_NSE, &nse));

        let mut frame = Vec::new();
        frame.extend_from_slice(&enc_len_delim(PD_SYMBOL_DATA, &sym));
        frame.extend_from_slice(&enc_len_delim(PD_POSITION_INFO, &info));

        let pd = decode_position_detail(&frame).expect("decode");
        let s = pd.symbol_data.expect("symbol data");
        assert_eq!(s.tr_time_stamp, 1_784_000_000_123);
        assert_eq!(s.search_id, "nifty-26jul-28500-ce");
        assert_eq!(product_name(s.stocks_product), Some("MIS"));
        assert_eq!(equity_type_name(s.equity_type), Some("OPTION"));
        assert_eq!(s.display_name, "NIFTY 28500 CE");
        assert_eq!(s.underlying_id, "nifty");
        assert_eq!(s.nse_market_lot, 75);
        assert_eq!(s.freeze_qty, 1800);
        assert_eq!(position_exchange_name(s.exchange), Some("NSE"));

        let info = pd.position_info.expect("position info");
        assert_eq!(info.symbol_isin, "INE000000000");
        assert!(info.bse.is_none(), "absent BSE leg stays None");
        let nse = info.nse.expect("nse leg");
        assert_eq!(nse.credit_qty, 75.0);
        assert_eq!(nse.credit_price, 152.35);
        assert_eq!(nse.debit_qty, 75.0);
        assert_eq!(nse.debit_price, 148.10);
    }

    /// Boundary: zero + negative double qty/price values on a position leg
    /// decode verbatim (never clamped, never panicking).
    #[test]
    fn test_decode_position_detail_zero_and_negative_qty_boundary() {
        let mut leg = Vec::new();
        leg.extend_from_slice(&enc_double(XP_CREDIT_QTY, 0.0));
        leg.extend_from_slice(&enc_double(XP_DEBIT_QTY, -25.0));
        leg.extend_from_slice(&enc_double(XP_DEBIT_PRICE, f64::MAX));
        let mut info = Vec::new();
        info.extend_from_slice(&enc_len_delim(PI_BSE, &leg));
        let frame = enc_len_delim(PD_POSITION_INFO, &info);

        let pd = decode_position_detail(&frame).expect("decode");
        let bse = pd
            .position_info
            .expect("position info")
            .bse
            .expect("bse leg");
        assert_eq!(bse.credit_qty, 0.0);
        assert_eq!(bse.debit_qty, -25.0);
        assert_eq!(bse.debit_price, f64::MAX);
        assert_eq!(bse.credit_price, 0.0, "absent field defaults to 0.0");
    }

    #[test]
    fn test_empty_inputs_decode_to_defaults_not_errors() {
        assert_eq!(
            decode_order_details_broadcast(&[]).expect("empty ok"),
            OrderDetailsBroadCastDto::default()
        );
        assert_eq!(
            decode_position_detail(&[]).expect("empty ok"),
            PositionDetailProto::default()
        );
    }

    /// Unknown fields (incl. the 17–21 descriptor gap) and unexpected wire
    /// types on known fields are skipped — forward compatible, never an error.
    #[test]
    fn test_unknown_fields_and_wire_types_are_skipped_forward_compat() {
        let mut b = Vec::new();
        b.extend_from_slice(&enc_int_field(17, 999)); // gap field, varint
        b.extend_from_slice(&enc_double(21, 7.5)); // gap field, fixed64
        b.extend_from_slice(&enc_len_delim(99, b"future")); // future field
        b.extend_from_slice(&enc_len_delim(OU_PRICE, b"wrong-wire")); // known field, wrong wire
        b.extend_from_slice(&enc_int_field(OU_PRICE, 4_200));
        let frame = enc_len_delim(OD_ORDER_DETAIL_UPDATE, &b);
        let od = decode_order_details_broadcast(&frame)
            .expect("decode")
            .order_detail
            .expect("present");
        assert_eq!(od.price_paise, 4_200);
    }

    #[test]
    fn test_truncated_inputs_error_not_panic() {
        // submessage claims 200 bytes but none follow
        let mut frame = enc_varint((OD_ORDER_DETAIL_UPDATE << 3) | u64::from(WIRE_LEN_DELIM));
        frame.extend_from_slice(&enc_varint(200));
        assert_eq!(
            decode_order_details_broadcast(&frame),
            Err(ProtoDecodeError::Truncated)
        );
        // varint runs off the end (lone continuation byte)
        assert_eq!(
            decode_position_detail(&[0x80]),
            Err(ProtoDecodeError::Truncated)
        );
        // fixed64 with 3 of 8 bytes
        let mut leg = enc_varint((XP_CREDIT_QTY << 3) | u64::from(WIRE_FIXED64));
        leg.extend_from_slice(&[1, 2, 3]);
        let frame = {
            let mut info = enc_len_delim(PI_BSE, &leg);
            let mut f = Vec::new();
            f.extend_from_slice(&enc_len_delim(PD_POSITION_INFO, &info));
            info.clear();
            f
        };
        assert_eq!(
            decode_position_detail(&frame),
            Err(ProtoDecodeError::Truncated)
        );
    }

    #[test]
    fn test_varint_overflow_and_unknown_wire_types_error_cleanly() {
        // 11 continuation bytes → overflow
        assert_eq!(
            decode_order_details_broadcast(&[0xff_u8; 11]),
            Err(ProtoDecodeError::VarintOverflow)
        );
        // wire types 3/4 (groups) and 6/7 are invalid → typed error
        for wire in [3u64, 4, 6, 7] {
            let frame = enc_varint((1 << 3) | wire);
            assert_eq!(
                decode_order_details_broadcast(&frame),
                Err(ProtoDecodeError::UnknownWireType(wire as u8))
            );
        }
    }

    #[test]
    fn test_huge_len_delim_is_truncated_not_oom() {
        let mut frame = enc_varint((PD_SYMBOL_DATA << 3) | u64::from(WIRE_LEN_DELIM));
        frame.extend_from_slice(&enc_varint(u64::MAX)); // absurd length
        assert_eq!(
            decode_position_detail(&frame),
            Err(ProtoDecodeError::Truncated)
        );
    }

    /// Non-UTF-8 string bytes are lossy-decoded, never an error (tolerance).
    #[test]
    fn test_non_utf8_string_is_lossy_decoded_not_error() {
        let mut b = Vec::new();
        b.extend_from_slice(&enc_len_delim(OU_REMARK, &[0xff, 0xfe, b'o', b'k']));
        let frame = enc_len_delim(OD_ORDER_DETAIL_UPDATE, &b);
        let od = decode_order_details_broadcast(&frame)
            .expect("decode")
            .order_detail
            .expect("present");
        assert!(od.remark.ends_with("ok"));
    }

    #[test]
    fn test_order_status_name_known_and_negative_unknown() {
        assert_eq!(order_status_name(0), Some("NEW"));
        assert_eq!(order_status_name(6), Some("EXECUTED"));
        assert_eq!(order_status_name(9), Some("CANCELLATION_REQUESTED"));
        assert_eq!(order_status_name(11), Some("COMPLETED"));
        assert_eq!(order_status_name(12), None, "open set — future value");
        assert_eq!(order_status_name(-1), None);
    }

    #[test]
    fn test_duration_name_known_and_negative_unknown() {
        assert_eq!(duration_name(0), Some("IOC"));
        assert_eq!(duration_name(1), Some("DAY"));
        assert_eq!(duration_name(4), Some("EOS"));
        assert_eq!(duration_name(5), None);
        assert_eq!(duration_name(-1), None);
    }

    #[test]
    fn test_exchange_name_known_and_negative_unknown() {
        assert_eq!(exchange_name(0), Some("BSE"));
        assert_eq!(exchange_name(1), Some("NSE"));
        assert_eq!(exchange_name(5), Some("US"), "ORDER file: 5 = US");
        assert_eq!(exchange_name(6), None);
        assert_eq!(exchange_name(-1), None);
    }

    #[test]
    fn test_segment_name_known_and_negative_unknown() {
        assert_eq!(segment_name(0), Some("CASH"));
        assert_eq!(segment_name(1), Some("FNO"));
        assert_eq!(segment_name(3), Some("COMMODITY"));
        assert_eq!(segment_name(4), None);
        assert_eq!(segment_name(-1), None);
    }

    #[test]
    fn test_product_name_known_and_negative_unknown() {
        assert_eq!(product_name(0), Some("CNC"));
        assert_eq!(product_name(1), Some("MIS"));
        assert_eq!(product_name(6), Some("MTF"));
        assert_eq!(product_name(7), None, "position-file _unknown stays None");
        assert_eq!(product_name(-1), None);
    }

    #[test]
    fn test_order_type_name_known_and_negative_unknown() {
        assert_eq!(order_type_name(0), Some("MKT"));
        assert_eq!(order_type_name(1), Some("L"));
        assert_eq!(order_type_name(3), Some("SL_M"));
        assert_eq!(order_type_name(4), None);
        assert_eq!(order_type_name(-1), None);
    }

    #[test]
    fn test_buy_sell_name_known_and_negative_unknown() {
        assert_eq!(buy_sell_name(0), Some("B"));
        assert_eq!(buy_sell_name(1), Some("S"));
        assert_eq!(buy_sell_name(2), None);
        assert_eq!(buy_sell_name(-1), None);
    }

    #[test]
    fn test_equity_type_name_known_and_negative_unknown() {
        assert_eq!(equity_type_name(0), Some("STOCKS"));
        assert_eq!(equity_type_name(1), Some("FUTURE"));
        assert_eq!(equity_type_name(2), Some("OPTION"));
        assert_eq!(equity_type_name(5), Some("BONDS"));
        assert_eq!(equity_type_name(6), None);
        assert_eq!(equity_type_name(-1), None);
    }

    #[test]
    fn test_position_exchange_name_known_and_negative_unknown() {
        assert_eq!(position_exchange_name(1), Some("NSE"));
        assert_eq!(
            position_exchange_name(5),
            Some("GLOBAL"),
            "POSITION file diverges: 5 = GLOBAL"
        );
        assert_eq!(position_exchange_name(6), Some("US"));
        assert_eq!(position_exchange_name(7), None);
        assert_eq!(position_exchange_name(-1), None);
    }

    #[test]
    fn test_decode_error_display_is_stable() {
        assert!(
            ProtoDecodeError::Truncated
                .to_string()
                .contains("truncated")
        );
        assert!(
            ProtoDecodeError::UnknownWireType(3)
                .to_string()
                .contains("wire type 3")
        );
    }
}
