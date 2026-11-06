//! Pins the dependency-light constants of `operator_movers_universe` (which
//! the operator Lambda compiles through `#[path]` and so cannot name
//! `crate::constants`) to the common constants they copy. Plan PR MV.

use tickvault_common::constants::{
    EXCHANGE_SEGMENT_NSE_EQ, IST_UTC_OFFSET_SECONDS_I64, TICK_PERSIST_END_SECS_OF_DAY_IST,
    TICK_PERSIST_START_SECS_OF_DAY_IST,
};
use tickvault_common::operator_movers_universe::{
    UNIVERSE_IST_OFFSET_SECS, UNIVERSE_NSE_EQ_SEGMENT, UNIVERSE_NSE_EQ_SEGMENT_CODE,
    UNIVERSE_SESSION_END_SECS_OF_DAY_IST, UNIVERSE_SESSION_START_SECS_OF_DAY_IST,
};

#[test]
fn test_universe_constants_match_the_common_constants() {
    assert_eq!(UNIVERSE_NSE_EQ_SEGMENT_CODE, EXCHANGE_SEGMENT_NSE_EQ);
    assert_eq!(
        tickvault_common::segment::segment_code_to_str(UNIVERSE_NSE_EQ_SEGMENT_CODE),
        UNIVERSE_NSE_EQ_SEGMENT
    );
    assert_eq!(UNIVERSE_IST_OFFSET_SECS, IST_UTC_OFFSET_SECONDS_I64);
    assert_eq!(
        UNIVERSE_SESSION_START_SECS_OF_DAY_IST,
        i64::from(TICK_PERSIST_START_SECS_OF_DAY_IST)
    );
    assert_eq!(
        UNIVERSE_SESSION_END_SECS_OF_DAY_IST,
        i64::from(TICK_PERSIST_END_SECS_OF_DAY_IST)
    );
}
