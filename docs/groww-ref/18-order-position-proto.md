# Groww Order/Position Push — Protobuf Wire Schemas (VERIFIED)

> **Status:** VERIFIED — field numbers, wire types, and enum values below were
> derived 2026-07-16 directly from the OFFICIAL `growwapi-1.5.0` PyPI wheel
> (`pip download growwapi==1.5.0 --no-deps`, wheel unzipped and the two
> generated `_pb2.py` modules' `serialized_pb` descriptors hand-decoded).
> This is the wheel-extraction evidence class (the §32/§35 sanctioned
> reference path — brutex code banned; the SDK wheel is the protocol source).
>
> **Consumers:** `crates/trading/src/oms/groww/push/proto.rs` (hand-written
> zero-dep decoders — order-push Stage B) routes MSG payloads delivered on the
> subjects in `crates/trading/src/oms/groww/push/subjects.rs`.
>
> **Companion refs:** `07-feed-websocket-streaming.md` (NATS-over-WS
> transport), `16-orders-margins-portfolio.md` (REST order surface — the
> enum VOCABULARY here matches its status set), `17-token-lifecycle.md`.

---

## 1. Evidence chain (what proves what)

| Artifact (inside the wheel) | Proves |
|---|---|
| `growwapi/groww/proto/stock_orders_socket_response_pb2.py` | `StockOrdersSocketResponse.proto` descriptor — `OrderDetailsBroadCastDto`, `OrderDetailUpdateDto`, `StageAndTimeStamp` field numbers/types + the order-file enums |
| `growwapi/groww/proto/position_socket_pb2.py` | `PositionSocket.proto` descriptor (package `groww.protobuf.fno.dto.fnoData`) — `PositionDetailProto`, `SymbolInfoProto`, `PositionInfoProto`, `BseProto`/`NseProto` + the position-file enums |
| `growwapi/groww/constants.py::FeedConstants` | The three push subject prefixes (verbatim): `stocks_fo/order/updates.apex.`, `stocks_fo/position/updates.apex.`, `stocks/order/updates.apex.` |
| `growwapi/groww/proto_parser.py` | MSG payloads on the order subjects parse directly as `OrderDetailsBroadCastDto`; on the position subject as `PositionDetailProto` (no envelope wrapper) |

---

## 2. `OrderDetailsBroadCastDto` (order/trade updates — both `stocks_fo/order/…` and `stocks/order/…`)

Top-level message of every order-update MSG payload.

| # | Field | Proto type | Notes |
|---|---|---|---|
| 1 | `stageAndTimeStamp` | `repeated StageAndTimeStamp` | lifecycle stage trail |
| 2 | `orderDetailUpdateDto` | `OrderDetailUpdateDto` | the order snapshot |

### 2.1 `StageAndTimeStamp`

| # | Field | Proto type | Notes |
|---|---|---|---|
| 1 | `timeStampFromMidNight` | `double` | fixed64; seconds-from-midnight style stamp |
| 2 | `stageName` | `string` | stage label |

### 2.2 `OrderDetailUpdateDto`

| # | Field | Proto type | Notes |
|---|---|---|---|
| 1 | `qty` | `int32` | |
| 2 | `price` | `int64` | **PAISE** (integer paise — the order-surface money convention) |
| 3 | `triggerPrice` | `int64` | paise |
| 4 | `filledQty` | `int32` | |
| 5 | `remainingQty` | `int32` | |
| 6 | `avgFillPrice` | `int64` | paise |
| 7 | `growwOrderId` | `string` | |
| 8 | `exchangeOrderId` | `string` | |
| 9 | `orderStatus` | `enum StocksOrderStatus` | |
| 10 | `duration` | `enum StocksOrderDuration` | |
| 11 | `exchange` | `enum StockExchange` (order file) | |
| 12 | `segment` | `enum StockSegment` | |
| 13 | `product` | `enum StocksProduct` | |
| 14 | `orderType` | `enum StocksOrderType` | |
| 15 | `buySell` | `enum StocksBuySell` | |
| 16 | `remark` | `string` | |
| 22 | `contractId` | `string` | note the GAP: 17–21 are UNASSIGNED in the descriptor |
| 23 | `guiOrderId` | `string` | |

**Field gap 17–21:** deliberately unassigned upstream (reserved/removed). The
decoder MUST skip-unknown (forward-compat) — a future Groww assignment of
17–21 must degrade to skipped fields, never a parse failure.

### 2.3 Order-file enums (all `enum` = varint on the wire)

| Enum | Values |
|---|---|
| `StocksOrderStatus` | NEW=0, ACKED=1, TRIGGER_PENDING=2, APPROVED=3, REJECTED=4, FAILED=5, EXECUTED=6, DELIVERY_AWAITED=7, CANCELLED=8, CANCELLATION_REQUESTED=9, MODIFICATION_REQUESTED=10, COMPLETED=11 |
| `StocksOrderDuration` | IOC=0, DAY=1, GTD=2, GTC=3, EOS=4 |
| `StockExchange` (order file) | BSE=0, NSE=1, MCX=2, MCXSX=3, NCDEX=4, US=5 |
| `StockSegment` | CASH=0, FNO=1, CURRENCY=2, COMMODITY=3 |
| `StocksProduct` | CNC=0, MIS=1, CO=2, BO=3, NRML=4, ARB=5, MTF=6 |
| `StocksOrderType` | MKT=0, L=1, SL=2, SL_M=3 |
| `StocksBuySell` | B=0, S=1 |
| `StocksTradeStatus` | EXECUTED=0, DELIVERY_AWAITED=1, COMPLETED=2, CANCELLED=3 |
| `OrderSource` | USER=0, SYSTEM=1, SMALLCASE=2, SMARTORDER=3, NA=4 |
| `StocksAmoStatus` | NA=0, PENDING=1, DISPATCHED=2, PARKED=3, PLACED=4, FAILED=5, MARKET=6 |

The `StocksOrderStatus` vocabulary matches the REST order-book open-set
(`16-orders-margins-portfolio.md`) — the push channel and the poll channel
speak the same status names. Rust-side mapping stays OPEN-SET (unknown enum
value → `None`, never a panic — annexure rule 15 discipline).

---

## 3. `PositionDetailProto` (position updates — `stocks_fo/position/…`)

Package `groww.protobuf.fno.dto.fnoData`. Top-level message of every
position-update MSG payload.

| # | Field | Proto type |
|---|---|---|
| 1 | `symbolData` | `SymbolInfoProto` |
| 2 | `positionInfo` | `PositionInfoProto` |

### 3.1 `SymbolInfoProto`

| # | Field | Proto type |
|---|---|---|
| 1 | `trTimeStamp` | `int64` |
| 2 | `searchId` | `string` |
| 3 | `stocksProduct` | `enum StocksProduct` (position file) |
| 4 | `contractId` | `string` |
| 5 | `equityType` | `enum EquityType` |
| 6 | `displayName` | `string` |
| 7 | `underlyingId` | `string` |
| 8 | `nseMarketLot` | `int64` |
| 9 | `bseMarketLot` | `int64` |
| 10 | `underlyingAssetType` | `enum EquityAsset` |
| 11 | `freezeQty` | `int64` |
| 12 | `exchange` | `enum StockExchange` (position file) |

### 3.2 `PositionInfoProto`

| # | Field | Proto type |
|---|---|---|
| 1 | `symbolIsin` | `string` |
| 2 | `BSE` | `BseProto` |
| 3 | `NSE` | `NseProto` |

### 3.3 `BseProto` / `NseProto` (identical shapes)

| # | Field | Proto type | Notes |
|---|---|---|---|
| 1 | `creditQty` | `double` | **position legs are DOUBLES** — only ORDER prices are int64 paise |
| 2 | `creditPrice` | `double` | |
| 3 | `debitQty` | `double` | |
| 4 | `debitPrice` | `double` | |

### 3.4 Position-file enums — DIVERGE from the order file (do NOT conflate)

| Enum | Values | Divergence |
|---|---|---|
| `StocksProduct` (position file) | CNC=0, MIS=1, CO=2, BO=3, NRML=4, ARB=5, MTF=6, `_unknown`=7 | adds `_unknown=7` |
| `StockExchange` (position file) | BSE=0, NSE=1, MCX=2, MCXSX=3, NCDEX=4, GLOBAL=5, US=6 | order file has US=5 and NO GLOBAL — the SAME numeric value 5 means US on the order channel and GLOBAL on the position channel |
| `EquityType` | STOCKS=0, FUTURE=1, OPTION=2, ETF=3, INDEX=4, BONDS=5 | position file only |
| `EquityAsset` | EQUITY_ASSET_STOCKS=0, EQUITY_ASSET_INDICES=1 | position file only |

**The value-5 collision is the trap:** exchange mappers MUST be per-channel
(`exchange_name` for order payloads vs `position_exchange_name` for position
payloads in `push/proto.rs`) — a shared mapper would mislabel US/GLOBAL.

---

## 4. Honest envelope

- Field numbers/types/enums: **VERIFIED** from the official wheel descriptors
  (not inferred from samples).
- LIVE delivery behavior (cadence, which fields are populated per stage,
  whether equity order updates share every field) is **UNVERIFIED-LIVE** —
  the first authorized push session is the probe.
- Decoders are tolerant by construction: unknown fields skipped, unknown enum
  values preserved as raw i32 with `None` name mapping, lossy UTF-8 accepted
  on strings — a vendor schema drift degrades to a coded decode signal
  (`GROWW-PUSH-03`), never a panic.
