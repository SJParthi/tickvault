//! The maps probed once per tick hash with `ahash`, not the standard
//! library's SipHash (audit plan PR10).
//!
//! # Why a source pin
//!
//! Dropping the third type parameter compiles cleanly: `HashMap<K, V>` falls
//! back to SipHash, and `HashMap::with_capacity` works again. Nothing else
//! would notice. Each pin names the declaration, so a revert fails here.
//!
//! # What the switch bought (MEASURED 2026-10-08)
//!
//! Nothing measurable on the candle fold: `benches/candle_fold.rs` in the
//! trading crate moved 712 → 687 µs per 500 ticks on a 20,000-instrument
//! table (p = 0.20) and 703 → 672 µs on 500 (p = 0.45), both inside noise.
//! A profile of the same bench puts about three quarters of the fold in the
//! f32-to-f64 price widening, not in the hash probe. The pin stays because
//! `ahash` is no slower, still keys at random per process, and is the hasher
//! `TickGapDetector` already uses, so the per-tick maps agree.

const AGGREGATOR: &str = include_str!("../../trading/src/candles/multi_tf_aggregator.rs");
const PREV_CLOSE: &str = include_str!("../src/prev_close_store.rs");
const SPOT_PRICE: &str = include_str!("../src/spot_price_store.rs");
const VOLUME_BOARD: &str = include_str!("../src/volume_leaderboard.rs");
const CONTRACT_MAP: &str = include_str!("../src/contract_underlying_map.rs");

/// (file label, source, declaration that must be present verbatim)
const PINS: [(&str, &str, &str); 6] = [
    (
        "multi_tf_aggregator.rs",
        AGGREGATOR,
        "index: HashMap<CompositeKey, u32, ahash::RandomState>,",
    ),
    (
        "prev_close_store.rs",
        PREV_CLOSE,
        "closes: HashMap<PrevCloseKey, f64, ahash::RandomState>,",
    ),
    (
        "spot_price_store.rs",
        SPOT_PRICE,
        "prices: PapayaHashMap<SpotPriceKey, AtomicU64, ahash::RandomState>,",
    ),
    (
        "volume_leaderboard.rs",
        VOLUME_BOARD,
        "volumes: HashMap<ContractKey, Tracked, ahash::RandomState>,",
    ),
    (
        "contract_underlying_map.rs",
        CONTRACT_MAP,
        "pub type OwnerMap = HashMap<ContractKey, ContractOwner, ahash::RandomState>;",
    ),
    (
        "contract_underlying_map.rs",
        CONTRACT_MAP,
        "inner: Arc<ArcSwap<OwnerMap>>,",
    ),
];

#[test]
fn per_tick_maps_use_ahash() {
    for (file, source, declaration) in PINS {
        assert!(
            source.contains(declaration),
            "{file} no longer declares `{declaration}`. A per-tick map fell back \
             to SipHash, or the declaration was renamed without moving this pin \
             (audit plan PR10)."
        );
    }
}

#[test]
fn the_pins_bite_on_a_sip_hash_declaration() {
    // The check above is a substring match; prove it rejects the SipHash
    // form of each pinned line, so a pin can never pass vacuously.
    for (file, _, declaration) in PINS {
        let reverted = declaration.replace(", ahash::RandomState>", ">").replace(
            "ArcSwap<OwnerMap>",
            "ArcSwap<HashMap<ContractKey, ContractOwner>>",
        );
        assert_ne!(
            reverted, declaration,
            "{file}: the revert form of `{declaration}` is identical to it"
        );
        assert!(
            !reverted.contains(declaration),
            "{file}: the SipHash form would still satisfy the pin"
        );
    }
}
