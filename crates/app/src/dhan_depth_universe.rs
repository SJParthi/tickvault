//! Depth-20 / depth-200 instrument selection, sourced from the per-minute
//! option chain.
//!
//! Authorized by the operator 2026-08-11 (second quote of that day): *"ensure
//! to enable connect estbalish al lteh 16 ocnenctions defintitley ddue okay?"*
//! The verbatim quote and the constraint analysis live in
//! `.claude/rules/project/websocket-connection-scope-lock.md`.
//!
//! # Why the option chain is the only legal source
//!
//! Depth needs a *tradeable* order book, and an index does not have one — so
//! the 4 hardcoded index SIDs the main feed carries can never populate depth.
//! Reaching real contracts needs contract-level `security_id`s, and there are
//! exactly three ways to get them:
//!
//! | Source | Rule status | Automation |
//! |---|---|---|
//! | Dhan instrument-master CSV | allowed again since the 2026-08-11 third quote | daily, but a whole extra parse for ids we already receive |
//! | A hardcoded contract list | permitted by the letter of Q3 | **fails** the standing no-manual-intervention mandate — option contracts expire weekly, so it needs a human edit every week and goes silently stale between them |
//! | **The per-minute option chain** | already authorized, already running | automatic and self-rolling |
//!
//! The third is the only one that satisfies both the quote and the
//! zero-manual-intervention rule at the same time. `POST /v2/optionchain`
//! returns a per-leg `security_id` (`docs/dhan-ref/06-option-chain.md:195`:
//! *"gives you the SecurityId of each option contract directly, no instrument
//! master lookup needed for subscriptions"*), we already parse it, and we
//! already persist it every minute as `option_chain_1m.contract_security_id`.
//! When the expiry rolls, the chain returns the new contracts and this
//! selection follows — with no code change and no human edit.
//!
//! # What this deliberately does NOT cover
//!
//! **Index FUTURES depth is unreachable and is not attempted here.**
//! `/v2/optionchain` returns `ce`/`pe` legs only; the expiry-list endpoint
//! returns dates, not ids; and `index_futures.rs` is a pure date filter fed by
//! the *Groww* master, whose `exchange_token` is a different id space from a
//! Dhan `security_id`. So a claim that "all 16 sockets carry data" must not be
//! read as including futures depth. Option depth is what this can honestly
//! deliver.

use tickvault_common::types::{ExchangeSegment, SecurityId};
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

/// Strikes each side of at-the-money that depth-20 subscribes, per underlying.
///
/// Matches the retired depth selector's ATM ± 10 (`CLAUDE.md`, "depth strike
/// selector (ATM ± 10)") rather than inventing a new number. 21 strikes × 2
/// legs × 3 underlyings = 126 instruments, comfortably inside the 250-slot
/// depth-20 envelope, which leaves headroom for a fourth underlying later
/// without touching the connection budget.
pub const DEPTH_20_ATM_STRIKES_EACH_SIDE: usize = 10;

/// Instruments depth-200 subscribes, across all underlyings.
///
/// depth-200 allows ONE instrument per connection and 5 connections, so 5 is
/// the vendor ceiling AND the budget — an odd number against a market that
/// trades in CE/PE pairs. Filling it means 2 whole pairs plus one lone leg.
///
/// **This deliberately reverses an earlier decision, so the reversal is
/// recorded rather than quietly applied.** The constant used to be
/// `DEPTH_200_MAX_PAIRS = 2`, taking 4 sockets and leaving the 5th empty, on
/// the stated grounds that a lone leg "cannot answer the spread and fill
/// questions this data exists for" and would be "analytically unusable".
///
/// That reasoning was half right and overstated the half it got wrong. A
/// 200-level book on a lone leg genuinely cannot answer CE-vs-PE questions —
/// synthetic parity, straddle spread, relative skew — because those need both
/// legs at the same strike simultaneously. But the questions a 200-level book
/// answers MOST of the time are within-leg: how much size rests at each of 200
/// levels, where the real liquidity cliff is, what a large order would sweep.
/// A lone leg answers all of those completely. "Unusable" was the wrong word;
/// "narrower" was the right one, and the difference decides whether the 5th
/// socket is worth opening.
///
/// So the 5th socket is filled with the next-nearest-ATM CE — see
/// [`DepthSelection::depth_200_lone_leg`], which reports whether a lone leg
/// was taken so the limitation is visible at the point of USE rather than
/// only here in a comment.
pub const DEPTH_200_MAX_SOCKETS: usize = 5;

/// One contract from a chain snapshot, before selection.
#[derive(Debug, Clone, PartialEq)]
pub struct DepthCandidate {
    /// Underlying symbol as the chain reports it (`NIFTY`, `BANKNIFTY`, …).
    pub underlying: String,
    /// The CONTRACT's own Dhan `security_id` — the thing depth subscribes.
    pub contract_security_id: i64,
    /// Expiry, epoch micros. Used to keep only the nearest expiry per
    /// underlying; a chain can legitimately carry weeklies and monthlies at
    /// once, and depth on a far month is bandwidth spent on an illiquid book.
    pub expiry_micros: i64,
    /// Strike price.
    pub strike: f64,
    /// Underlying spot at snapshot time — the reference for "how far from ATM".
    pub spot: f64,
    /// `CE` or `PE`.
    pub leg: String,
}

/// The chosen depth sets, plus an honest account of everything refused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DepthSelection {
    /// depth-20 subscription set.
    pub depth_20: Vec<SubscribeInstrument>,
    /// depth-200 subscription set.
    pub depth_200: Vec<SubscribeInstrument>,
    /// Whether the last depth-200 socket carries a LONE leg (no partner).
    ///
    /// The 5-socket budget is odd and options trade in pairs, so a full
    /// budget always ends in one unpartnered leg. That leg's 200-level book
    /// answers every within-leg question (resting size per level, the
    /// liquidity cliff, sweep cost) and answers NO cross-leg question
    /// (synthetic parity, straddle spread, relative skew) because its
    /// partner is not subscribed.
    ///
    /// Surfaced as a field rather than left in a comment so any consumer that
    /// computes a cross-leg quantity can see that the data cannot support it,
    /// instead of computing a number from a book whose other half is absent.
    pub depth_200_lone_leg: bool,
    /// Rows whose `contract_security_id` was 0 or negative.
    ///
    /// The chain parser defaults this field to `0` when absent, and the field
    /// is marked "added v2.5" upstream — so a zero here means "the vendor did
    /// not tell us the id", NOT "instrument zero". Subscribing it would send a
    /// well-formed request for a nonexistent instrument and receive silence
    /// that looks exactly like a quiet book.
    pub refused_zero_id: usize,
    /// Rows whose underlying has no known contract segment.
    pub refused_unknown_underlying: usize,
    /// Rows whose contract segment cannot carry market depth AT ALL.
    ///
    /// Dhan's Full Market Depth is NSE-only —
    /// `docs/dhan-ref/04-full-market-depth-websocket.md:13` ("Only NSE Equity
    /// and Derivatives segments supported") and `:274` ("No BSE, MCX, or
    /// currency") — and that overview line governs the 20-level and 200-level
    /// endpoints alike. SENSEX options are `BSE_FNO`, so **no amount of
    /// configuration can give SENSEX a depth book**; it is a vendor
    /// limitation, not a budget one.
    ///
    /// Refusing here rather than at subscribe time is the whole point. Before
    /// this counter existed the selector handed BSE_FNO contracts to the
    /// planner, which opened a socket for them; the depth-200 builder then
    /// refused the payload and the connection was torn down — one of the five
    /// authorized 200-level sockets spent, live, on an instrument that can
    /// never deliver (observed in prod 2026-08-12 09:06:49 IST, `WS-GAP-02`,
    /// `reason: "... got: BSE_FNO"`). The depth-20 path was worse: it had no
    /// such builder check, so the same contracts went on the wire and came
    /// back as silence, which is indistinguishable from a quiet book.
    pub refused_depth_ineligible_segment: usize,
    /// Rows with a non-finite or non-positive strike/spot.
    pub refused_bad_price: usize,
}

/// Map an underlying to the segment its OPTION CONTRACTS trade in.
///
/// # Why this mapping has to exist at all
///
/// The chain response does not carry the contract's segment. What
/// `option_chain_1m.exchange_segment` stores is the UNDERLYING's segment —
/// `IDX_I`, hardcoded at `option_chain_1m_persistence.rs:108` — because the
/// underlying is an index. Subscribing depth with `IDX_I` would be a
/// well-formed request for the wrong instrument.
///
/// So the contract segment is OUR inference, not a vendor fact, and it is
/// written here once, tested, and **fail-closed**: an underlying this does not
/// recognise is refused and counted, never guessed. Guessing would be
/// especially seductive here because two of the three map to the same value —
/// a default of `NseFno` would be right twice and silently wrong for SENSEX,
/// which is precisely the kind of wrongness that survives review.
#[must_use]
pub fn contract_segment_for_underlying(underlying: &str) -> Option<ExchangeSegment> {
    match underlying.trim().to_ascii_uppercase().as_str() {
        "NIFTY" | "BANKNIFTY" | "MIDCPNIFTY" | "FINNIFTY" => Some(ExchangeSegment::NseFno),
        "SENSEX" | "BANKEX" => Some(ExchangeSegment::BseFno),
        _ => None,
    }
}

/// Whether a segment can carry Dhan market depth at all.
///
/// This is a VENDOR limitation, not ours and not a budget choice:
/// `docs/dhan-ref/04-full-market-depth-websocket.md:13` states "Only NSE
/// Equity and Derivatives segments supported" in the OVERVIEW — above the
/// 20-level and 200-level endpoint sections both — and `:274` repeats it as
/// "No BSE, MCX, or currency".
///
/// The consequence worth stating plainly: **SENSEX can never have a depth
/// book.** SENSEX options trade in `BSE_FNO`, so no config flag, no extra
/// socket, and no operator authorization can produce depth for it. Any plan
/// that counts a SENSEX depth socket is counting a socket that dies on
/// connect.
///
/// Mirrors `subscription_builder::validate_depth_segment`, deliberately.
/// That function is the last line of defence at payload-build time; this one
/// is the first, at SELECTION time — and the two must agree. A divergence is
/// caught by `test_segment_supports_depth_matches_the_builders_own_guard`,
/// which runs both over every segment.
#[must_use]
pub const fn segment_supports_depth(segment: ExchangeSegment) -> bool {
    matches!(
        segment,
        ExchangeSegment::NseEquity | ExchangeSegment::NseFno
    )
}

/// Distance from at-the-money. `f64::MAX` for unusable prices so they sort last
/// and are refused before selection rather than silently ranked first.
fn atm_distance(candidate: &DepthCandidate) -> f64 {
    if !candidate.strike.is_finite()
        || !candidate.spot.is_finite()
        || candidate.strike <= 0.0
        || candidate.spot <= 0.0
    {
        return f64::MAX;
    }
    (candidate.strike - candidate.spot).abs()
}

/// Choose the depth-20 and depth-200 sets from one chain snapshot.
///
/// Pure, so every refusal rule is testable without a database or a socket.
///
/// Selection, per underlying:
/// 1. keep only the NEAREST expiry (a far month is an illiquid book);
/// 2. rank strikes by distance from spot;
/// 3. depth-20 takes ATM ± [`DEPTH_20_ATM_STRIKES_EACH_SIDE`], both legs;
/// 4. depth-200 takes whole CE/PE pairs nearest ATM, up to
///    [`DEPTH_200_MAX_PAIRS`] across ALL underlyings.
#[must_use]
pub fn select_depth_universe(candidates: &[DepthCandidate]) -> DepthSelection {
    let mut out = DepthSelection::default();

    // ── Refuse first, so nothing unusable can reach the ranking. ──
    let mut usable: Vec<(&DepthCandidate, ExchangeSegment)> = Vec::new();
    for c in candidates {
        if c.contract_security_id <= 0 {
            out.refused_zero_id += 1;
            continue;
        }
        let Some(segment) = contract_segment_for_underlying(&c.underlying) else {
            out.refused_unknown_underlying += 1;
            continue;
        };
        if !segment_supports_depth(segment) {
            out.refused_depth_ineligible_segment += 1;
            continue;
        }
        if atm_distance(c) == f64::MAX {
            out.refused_bad_price += 1;
            continue;
        }
        usable.push((c, segment));
    }

    // ── Group by underlying, keeping only its nearest expiry. ──
    let mut underlyings: Vec<String> = usable
        .iter()
        .map(|(c, _)| c.underlying.trim().to_ascii_uppercase())
        .collect();
    underlyings.sort_unstable();
    underlyings.dedup();

    // Whole CE/PE pairs for depth-200, ranked across every underlying, so the
    // 2 pairs go to the 2 most at-the-money books rather than to whichever
    // underlying happens to sort first.
    let mut pair_pool: Vec<(f64, SubscribeInstrument, SubscribeInstrument)> = Vec::new();

    for underlying in &underlyings {
        let mut rows: Vec<(&DepthCandidate, ExchangeSegment)> = usable
            .iter()
            .filter(|(c, _)| &c.underlying.trim().to_ascii_uppercase() == underlying)
            .copied()
            .collect();
        if rows.is_empty() {
            continue;
        }
        let nearest_expiry = rows.iter().map(|(c, _)| c.expiry_micros).min().unwrap_or(0);
        rows.retain(|(c, _)| c.expiry_micros == nearest_expiry);

        // Distinct strikes, nearest-ATM first.
        let mut strikes: Vec<f64> = rows.iter().map(|(c, _)| c.strike).collect();
        strikes.sort_by(|a, b| a.total_cmp(b));
        strikes.dedup_by(|a, b| (*a - *b).abs() < f64::EPSILON);
        strikes.sort_by(|a, b| {
            let da = rows
                .iter()
                .find(|(c, _)| (c.strike - *a).abs() < f64::EPSILON)
                .map_or(f64::MAX, |(c, _)| atm_distance(c));
            let db = rows
                .iter()
                .find(|(c, _)| (c.strike - *b).abs() < f64::EPSILON)
                .map_or(f64::MAX, |(c, _)| atm_distance(c));
            da.total_cmp(&db)
        });

        let keep = DEPTH_20_ATM_STRIKES_EACH_SIDE * 2 + 1;
        for (rank, strike) in strikes.iter().enumerate() {
            let legs: Vec<(&DepthCandidate, ExchangeSegment)> = rows
                .iter()
                .filter(|(c, _)| (c.strike - *strike).abs() < f64::EPSILON)
                .copied()
                .collect();

            if rank < keep {
                for (c, segment) in &legs {
                    out.depth_20.push(SubscribeInstrument {
                        security_id: c.contract_security_id as SecurityId,
                        segment: *segment,
                    });
                }
            }

            // A pair needs BOTH legs at this strike; a lone leg is skipped
            // rather than promoted, per the never-split-a-pair rule above.
            let ce = legs.iter().find(|(c, _)| c.leg.eq_ignore_ascii_case("CE"));
            let pe = legs.iter().find(|(c, _)| c.leg.eq_ignore_ascii_case("PE"));
            if let (Some((ce_c, ce_seg)), Some((pe_c, pe_seg))) = (ce, pe) {
                pair_pool.push((
                    atm_distance(ce_c),
                    SubscribeInstrument {
                        security_id: ce_c.contract_security_id as SecurityId,
                        segment: *ce_seg,
                    },
                    SubscribeInstrument {
                        security_id: pe_c.contract_security_id as SecurityId,
                        segment: *pe_seg,
                    },
                ));
            }
        }
    }

    // ── depth-200: whole pairs first, then one lone leg for the odd socket. ──
    //
    // Pairs are taken nearest-ATM first so the two complete books are the two
    // most informative ones. Only once no WHOLE pair fits in the remaining
    // budget does a lone leg get taken — so the lone leg is always the LAST
    // socket, never a pair displaced by a half.
    pair_pool.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut remaining_pairs = pair_pool.into_iter();
    for (_, ce, pe) in remaining_pairs.by_ref() {
        // `+ 2` because a pair costs two sockets; a pair that would overflow
        // the budget is skipped, and the loop falls through to the lone-leg
        // step below rather than truncating the pair here.
        if out.depth_200.len().saturating_add(2) > DEPTH_200_MAX_SOCKETS {
            // The CE of this rejected pair is the nearest-ATM leg still
            // unspent, which makes it the best candidate for the odd socket.
            if out.depth_200.len() < DEPTH_200_MAX_SOCKETS {
                out.depth_200.push(ce);
                out.depth_200_lone_leg = true;
            }
            break;
        }
        out.depth_200.push(ce);
        out.depth_200.push(pe);
    }

    out
}

/// The `/exec` query selecting one row per live contract from the most recent
/// chain snapshot.
///
/// `LATEST ON ts PARTITION BY` collapses the per-minute history to the newest
/// row per contract, and the `expiry >= today` bound is what stops a dead
/// contract being subscribed after an expiry rolls: those rows stay in the
/// table forever (nothing evicts them) and a subscription to an expired
/// contract returns silence indistinguishable from a quiet book.
///
/// `contract_security_id > 0` is deliberately ALSO enforced in
/// [`select_depth_universe`], not only here — the SQL filter keeps the result
/// set small, the code filter is what makes the refusal countable.
#[must_use]
pub fn build_depth_candidate_query(today_ist_nanos: i64) -> String {
    let today_micros = today_ist_nanos / 1_000;
    format!(
        // `ts >= {today_micros}` added 2026-08-14. Without it `LATEST ON ts`
        // returns the newest row per partition from ANY day, so a pre-09:16
        // caller silently got YESTERDAY's chain — stale `underlying_spot`
        // driving ATM ranking, on derivative ids Dhan documents as unstable
        // across days.
        //
        // This bound is safe ONLY because the caller now retries
        // (`dhan_feed_stack::attach_depth_when_available`). Added alone, it
        // would have turned a boot-time read into zero rows and REDUCED the
        // socket count — correct but worse. Day bound and late-attach are one
        // change; do not separate them.
        "SELECT underlying_symbol, contract_security_id, expiry, strike, \
         underlying_spot, leg FROM option_chain_1m \
         WHERE feed = 'dhan' AND contract_security_id > 0 AND expiry >= {today_micros} \
         AND ts >= {today_micros} \
         LATEST ON ts PARTITION BY underlying_security_id, expiry, strike, leg;"
    )
}

/// Parse the `/exec` dataset into candidates.
///
/// Fail-LOUD on a malformed body or a missing `dataset` key, fail-soft per row
/// — the house pattern from `brutex_crossverify_boot::parse_lifecycle_dataset`.
/// The distinction matters: an empty `Vec` returned for garbage would be
/// indistinguishable from a genuinely empty chain, and the caller's response to
/// those two is different.
///
/// # Errors
/// Returns `Err` when the body is not JSON or carries no `dataset` array.
pub fn parse_depth_candidates_dataset(body: &str) -> Result<Vec<DepthCandidate>, String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Err("malformed /exec response: not valid JSON".to_owned());
    };
    let Some(rows) = v.get("dataset").and_then(|d| d.as_array()) else {
        return Err("malformed /exec response: missing dataset array".to_owned());
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(cols) = row.as_array() else { continue };
        if cols.len() < 6 {
            continue;
        }
        let underlying = cols[0].as_str().unwrap_or_default();
        if underlying.is_empty() {
            continue;
        }
        let Some(contract_security_id) = cols[1].as_i64() else {
            continue;
        };
        let expiry_micros = cols[2].as_i64().unwrap_or(0);
        let strike = cols[3].as_f64().unwrap_or(f64::NAN);
        let spot = cols[4].as_f64().unwrap_or(f64::NAN);
        let leg = cols[5].as_str().unwrap_or_default();
        if leg.is_empty() {
            continue;
        }
        out.push(DepthCandidate {
            underlying: underlying.to_owned(),
            contract_security_id,
            expiry_micros,
            strike,
            spot,
            leg: leg.to_owned(),
        });
    }
    Ok(out)
}

/// `/exec` HTTP timeout. Matches the sibling boot readers.
const QUESTDB_EXEC_TIMEOUT_SECS: u64 = 10;

/// Load and select the depth universe from the newest chain snapshot.
///
/// # Timing, stated plainly
///
/// This runs ONCE, at boot. On a normal morning the newest snapshot is
/// yesterday's final minute, whose contracts are still the live expiry — so
/// depth comes up populated before the open.
///
/// **The morning AFTER an expiry is the exception**: yesterday's rows are
/// excluded by the `expiry >= today` bound (correctly — they are dead
/// contracts), and today's chain has not been fetched yet at 08:30. Depth is
/// therefore EMPTY that morning until the process is next restarted. That is a
/// real limitation, not a bug being papered over: closing it needs live
/// re-subscription on an already-running pool, which is a larger change than
/// this one. It is logged at `error!` rather than left to be noticed.
///
/// An empty result is never reported as success. An empty instrument set opens
/// ZERO sockets, and calling that "depth enabled" is exactly the false-OK the
/// scope-lock forbids.
// Every decision this makes is delegated to the unit-tested pure fns
// (build_depth_candidate_query, parse_depth_candidates_dataset,
// select_depth_universe); this wrapper only moves bytes and logs.
// TEST-EXEMPT: network I/O (QuestDB /exec) — see the note above.
pub async fn load_depth_universe(
    questdb: &tickvault_common::config::QuestDbConfig,
    today_ist_nanos: i64,
) -> DepthSelection {
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(QUESTDB_EXEC_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            tracing::error!(
                ?err,
                "depth universe: HTTP client build failed — depth-20 and depth-200 will \
                 open ZERO sockets this session"
            );
            return DepthSelection::default();
        }
    };
    let sql = build_depth_candidate_query(today_ist_nanos);
    let body = match client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(b) => b,
            Err(err) => {
                tracing::error!(
                    ?err,
                    "depth universe: could not read the chain snapshot response — \
                     depth-20 and depth-200 will open ZERO sockets this session"
                );
                return DepthSelection::default();
            }
        },
        Ok(resp) => {
            tracing::error!(
                status = %resp.status(),
                "depth universe: chain snapshot query returned non-2xx — depth-20 and \
                 depth-200 will open ZERO sockets this session"
            );
            return DepthSelection::default();
        }
        Err(err) => {
            tracing::error!(
                ?err,
                "depth universe: chain snapshot query failed — depth-20 and depth-200 \
                 will open ZERO sockets this session"
            );
            return DepthSelection::default();
        }
    };

    let candidates = match parse_depth_candidates_dataset(&body) {
        Ok(c) => c,
        Err(reason) => {
            tracing::error!(
                reason,
                "depth universe: chain snapshot did not parse — depth-20 and depth-200 \
                 will open ZERO sockets this session"
            );
            return DepthSelection::default();
        }
    };

    let selection = select_depth_universe(&candidates);
    if selection.depth_20.is_empty() && selection.depth_200.is_empty() {
        tracing::error!(
            candidates = candidates.len(),
            refused_zero_id = selection.refused_zero_id,
            refused_unknown_underlying = selection.refused_unknown_underlying,
            refused_bad_price = selection.refused_bad_price,
            refused_depth_ineligible_segment = selection.refused_depth_ineligible_segment,
            "depth universe is EMPTY — depth-20 and depth-200 will open ZERO sockets \
             this session. Expected on the morning after an expiry (yesterday's \
             contracts are dead and today's chain has not been fetched yet); a restart \
             after 09:16 IST populates it. Any other time, check whether the \
             option-chain leg is running and whether contract_security_id is populated."
        );
    } else {
        tracing::info!(
            depth_20 = selection.depth_20.len(),
            depth_200 = selection.depth_200.len(),
            candidates = candidates.len(),
            refused_zero_id = selection.refused_zero_id,
            refused_unknown_underlying = selection.refused_unknown_underlying,
            refused_bad_price = selection.refused_bad_price,
            refused_depth_ineligible_segment = selection.refused_depth_ineligible_segment,
            "depth universe selected from the option chain"
        );
    }
    selection
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(underlying: &str, sid: i64, strike: f64, leg: &str) -> DepthCandidate {
        DepthCandidate {
            underlying: underlying.to_owned(),
            contract_security_id: sid,
            expiry_micros: 1_000,
            strike,
            spot: 100.0,
            leg: leg.to_owned(),
        }
    }

    /// Two of the three underlyings map to `NseFno`, so a default-to-NseFno
    /// would be right twice and silently wrong for SENSEX. That is exactly the
    /// shape of wrongness that survives review, which is why the map is
    /// fail-closed and this test names SENSEX explicitly.
    #[test]
    fn test_contract_segment_for_underlying_is_fail_closed_and_bse_is_not_defaulted() {
        assert_eq!(
            contract_segment_for_underlying("NIFTY"),
            Some(ExchangeSegment::NseFno)
        );
        assert_eq!(
            contract_segment_for_underlying("BANKNIFTY"),
            Some(ExchangeSegment::NseFno)
        );
        assert_eq!(
            contract_segment_for_underlying("SENSEX"),
            Some(ExchangeSegment::BseFno),
            "SENSEX options are BSE_FNO — a default of NseFno would be wrong here only"
        );
        assert_eq!(
            contract_segment_for_underlying("NOT_A_REAL_INDEX"),
            None,
            "unknown underlyings must be refused, never guessed"
        );
    }

    /// The chain parser defaults a missing `contract_security_id` to 0, and the
    /// field is "added v2.5" upstream. A zero means the vendor did not tell us
    /// the id — subscribing it sends a well-formed request for a nonexistent
    /// instrument and receives silence that reads exactly like a quiet book.
    #[test]
    fn test_select_depth_universe_refuses_and_counts_zero_contract_ids() {
        let rows = vec![
            candidate("NIFTY", 0, 100.0, "CE"),
            candidate("NIFTY", -1, 100.0, "PE"),
            candidate("NIFTY", 555, 100.0, "CE"),
        ];
        let sel = select_depth_universe(&rows);
        assert_eq!(sel.refused_zero_id, 2);
        assert!(
            sel.depth_20.iter().all(|i| i.security_id == 555),
            "only the real id may be subscribed"
        );
    }

    #[test]
    fn test_unknown_underlying_is_refused_and_counted() {
        let rows = vec![candidate("MYSTERY", 900, 100.0, "CE")];
        let sel = select_depth_universe(&rows);
        assert_eq!(sel.refused_unknown_underlying, 1);
        assert!(sel.depth_20.is_empty());
        assert!(sel.depth_200.is_empty());
    }

    /// depth-200 has 5 slots and options trade in pairs, so a full budget is
    /// 2 whole pairs plus one lone leg. Pair ORDER is what this pins: the two
    /// complete books must be the two nearest ATM, and the lone leg must be
    /// the LAST socket — never a pair displaced by a half.
    #[test]
    fn test_depth_200_fills_all_five_sockets_pairs_first_then_one_lone_leg() {
        let rows = vec![
            candidate("NIFTY", 1, 100.0, "CE"),
            candidate("NIFTY", 2, 100.0, "PE"),
            candidate("NIFTY", 3, 105.0, "CE"),
            candidate("NIFTY", 4, 105.0, "PE"),
            candidate("NIFTY", 5, 110.0, "CE"),
            candidate("NIFTY", 6, 110.0, "PE"),
        ];
        let sel = select_depth_universe(&rows);
        assert_eq!(
            sel.depth_200.len(),
            DEPTH_200_MAX_SOCKETS,
            "all five authorized 200-level sockets must be used"
        );
        // Nearest-ATM pairs (spot 100) are (1,2) then (3,4); the 5th socket
        // takes the CE of the next pair — id 5, not id 6.
        let ids: Vec<u64> = sel.depth_200.iter().map(|i| i.security_id).collect();
        assert_eq!(
            ids,
            vec![1, 2, 3, 4, 5],
            "two whole pairs first, then the nearest unspent CE"
        );
        assert!(
            sel.depth_200_lone_leg,
            "the lone leg must be REPORTED — a consumer computing a cross-leg \
             quantity from an unpartnered book would be computing from half \
             the data with no way to know it"
        );
    }

    /// A budget that only fits whole pairs must NOT report a lone leg.
    ///
    /// The flag is what a consumer keys its cross-leg logic off, so a flag
    /// that is set unconditionally is worse than no flag at all.
    #[test]
    fn test_lone_leg_flag_is_false_when_the_budget_holds_only_whole_pairs() {
        let rows = vec![
            candidate("NIFTY", 1, 100.0, "CE"),
            candidate("NIFTY", 2, 100.0, "PE"),
            candidate("NIFTY", 3, 105.0, "CE"),
            candidate("NIFTY", 4, 105.0, "PE"),
        ];
        let sel = select_depth_universe(&rows);
        assert_eq!(sel.depth_200.len(), 4, "only two pairs exist to take");
        assert!(
            !sel.depth_200_lone_leg,
            "no lone leg was taken, so the flag must stay false"
        );
    }

    /// A strike carrying only one leg is not a PAIR and must not be ranked as
    /// one. It may still reach the odd socket, but only via the lone-leg step
    /// after the pairs are placed — never ahead of a complete pair.
    #[test]
    fn test_a_half_strike_never_displaces_a_whole_pair() {
        let rows = vec![
            candidate("NIFTY", 1, 100.0, "CE"), // no PE at this strike
            candidate("NIFTY", 3, 105.0, "CE"),
            candidate("NIFTY", 4, 105.0, "PE"),
        ];
        let sel = select_depth_universe(&rows);
        let ids: Vec<u64> = sel.depth_200.iter().map(|i| i.security_id).collect();
        assert_eq!(
            ids,
            vec![3, 4],
            "the pair at 105 is taken; the lone CE at 100 is nearer ATM but is \
             not a pair, and with no second pair to reject there is no lone-leg \
             step to place it"
        );
        assert!(!sel.depth_200_lone_leg);
    }

    /// SENSEX can NEVER have a depth book, and the selector — not the socket —
    /// is where that has to be enforced.
    ///
    /// Regression for the 2026-08-12 prod defect: the selector handed BSE_FNO
    /// contracts to the planner, one of the five authorized 200-level sockets
    /// was opened for them, and the builder then refused the payload
    /// (`WS-GAP-02`, 09:06:49 IST) and tore the connection down. The depth-20
    /// path had no such check at all, so the same contracts went on the wire
    /// and came back as silence.
    #[test]
    fn test_bse_fno_is_refused_before_it_can_cost_a_socket() {
        let rows = vec![
            candidate("SENSEX", 10, 100.0, "CE"),
            candidate("SENSEX", 11, 100.0, "PE"),
            candidate("NIFTY", 1, 100.0, "CE"),
            candidate("NIFTY", 2, 100.0, "PE"),
        ];
        let sel = select_depth_universe(&rows);

        assert_eq!(
            sel.refused_depth_ineligible_segment, 2,
            "both SENSEX legs must be refused AND counted — a silent skip \
             would leave the operator unable to explain the missing underlying"
        );
        let all: Vec<u64> = sel
            .depth_20
            .iter()
            .chain(sel.depth_200.iter())
            .map(|i| i.security_id)
            .collect();
        assert_eq!(
            all,
            vec![1, 2, 1, 2],
            "only the NIFTY legs survive, in both depth sets"
        );
        assert!(
            sel.depth_20
                .iter()
                .chain(sel.depth_200.iter())
                .all(|i| i.segment != ExchangeSegment::BseFno),
            "no BSE_FNO instrument may reach either depth set"
        );
    }

    /// The selection-time gate and the payload-time gate must agree exactly.
    ///
    /// They are two functions in two crates encoding one vendor rule, which is
    /// precisely the shape that drifts. If a future edit widens one, this
    /// fails rather than letting the selector queue instruments the builder
    /// will refuse (a socket opened to die) or — worse — letting the builder
    /// accept what the selector would have rejected.
    #[test]
    fn test_segment_supports_depth_matches_the_builders_own_guard() {
        use tickvault_core::websocket::subscription_builder::validate_depth_segment;

        for segment in [
            ExchangeSegment::IdxI,
            ExchangeSegment::NseEquity,
            ExchangeSegment::NseFno,
            ExchangeSegment::BseEquity,
            ExchangeSegment::BseFno,
            ExchangeSegment::McxComm,
            ExchangeSegment::NseCurrency,
            ExchangeSegment::BseCurrency,
        ] {
            assert_eq!(
                segment_supports_depth(segment),
                validate_depth_segment(segment).is_ok(),
                "selection and payload gates disagree on {segment:?} — one of \
                 them will admit an instrument the other refuses"
            );
        }
    }

    /// A chain legitimately carries weeklies AND monthlies. Depth on a far
    /// month is bandwidth spent on an illiquid book, so only the nearest expiry
    /// is subscribed.
    #[test]
    fn test_only_the_nearest_expiry_is_selected() {
        let mut far = candidate("NIFTY", 99, 100.0, "CE");
        far.expiry_micros = 9_999_999;
        let rows = vec![candidate("NIFTY", 1, 100.0, "CE"), far];
        let sel = select_depth_universe(&rows);
        assert!(
            sel.depth_20.iter().all(|i| i.security_id == 1),
            "the far-month contract must not be subscribed"
        );
    }

    /// A NaN or non-positive price cannot be ranked against ATM. Ranking it
    /// anyway would sort it to an arbitrary position and could put garbage at
    /// the front of the depth-200 pair pool.
    #[test]
    fn test_non_finite_prices_are_refused_and_counted() {
        let mut nan_strike = candidate("NIFTY", 7, f64::NAN, "CE");
        nan_strike.spot = 100.0;
        let mut zero_spot = candidate("NIFTY", 8, 100.0, "PE");
        zero_spot.spot = 0.0;
        let sel = select_depth_universe(&[nan_strike, zero_spot]);
        assert_eq!(sel.refused_bad_price, 2);
        assert!(sel.depth_20.is_empty());
    }

    /// Ranking is by distance from spot, not by strike order — otherwise the
    /// "nearest ATM" set would just be the lowest strikes.
    #[test]
    fn test_depth_20_keeps_the_strikes_nearest_atm_not_the_lowest() {
        let mut rows = Vec::new();
        // Spot is 100; strikes 1..=60 means the lowest are the FURTHEST out.
        for i in 1..=60_i64 {
            let mut c = candidate("NIFTY", i, i as f64, "CE");
            c.spot = 100.0;
            rows.push(c);
        }
        let sel = select_depth_universe(&rows);
        let ids: Vec<u64> = sel.depth_20.iter().map(|i| i.security_id).collect();
        assert!(
            ids.contains(&60),
            "strike 60 is nearest spot 100 and must be kept: {ids:?}"
        );
        assert!(
            !ids.contains(&1),
            "strike 1 is furthest from spot and must be dropped: {ids:?}"
        );
        assert_eq!(
            ids.len(),
            DEPTH_20_ATM_STRIKES_EACH_SIDE * 2 + 1,
            "one leg per strike here, so the count is the strike window"
        );
    }

    /// Segments are resolved PER UNDERLYING, and the BSE answer is then what
    /// disqualifies SENSEX from depth entirely.
    ///
    /// **This test previously asserted the opposite** — that a SENSEX row
    /// reaches `depth_20` carrying `BSE_FNO`. It passed, and the behaviour it
    /// pinned was the 2026-08-12 prod defect: a `BSE_FNO` instrument in a
    /// depth set costs a socket that either dies on connect (depth-200) or
    /// sits live and silent (depth-20). The mapping half of the old assertion
    /// was right and is kept via `contract_segment_for_underlying`; the
    /// reaches-the-depth-set half was the bug and is now inverted.
    #[test]
    fn test_segments_are_per_underlying_and_bse_is_then_disqualified() {
        // The mapping itself still distinguishes them — that is what makes the
        // disqualification possible rather than a blanket refusal.
        assert_eq!(
            contract_segment_for_underlying("NIFTY"),
            Some(ExchangeSegment::NseFno)
        );
        assert_eq!(
            contract_segment_for_underlying("SENSEX"),
            Some(ExchangeSegment::BseFno)
        );

        let rows = vec![
            candidate("NIFTY", 11, 100.0, "CE"),
            candidate("SENSEX", 22, 100.0, "CE"),
        ];
        let sel = select_depth_universe(&rows);

        let nifty = sel
            .depth_20
            .iter()
            .find(|i| i.security_id == 11)
            .expect("NIFTY is NSE_FNO and depth-eligible");
        assert_eq!(nifty.segment, ExchangeSegment::NseFno);

        assert!(
            sel.depth_20.iter().all(|i| i.security_id != 22),
            "SENSEX must NOT reach the depth set — Dhan serves no BSE depth, \
             so this socket could only ever be silent"
        );
        assert_eq!(sel.refused_depth_ineligible_segment, 1);
    }

    /// Garbage must NOT read as "the chain is empty" — the caller treats those
    /// two very differently, and conflating them is how a parse failure
    /// becomes a silent zero-socket depth lane.
    #[test]
    fn test_parse_depth_candidates_dataset_errors_on_garbage_not_empty_list() {
        assert!(parse_depth_candidates_dataset("not json").is_err());
        assert!(parse_depth_candidates_dataset("{}").is_err());
        assert_eq!(
            parse_depth_candidates_dataset(r#"{"dataset":[]}"#),
            Ok(vec![]),
            "a genuinely empty chain stays Ok(empty)"
        );
    }

    #[test]
    fn test_dataset_row_parses_into_a_candidate() {
        let body = r#"{"dataset":[["NIFTY",4242,1780000000000,25000.0,24980.5,"CE"]]}"#;
        let rows = parse_depth_candidates_dataset(body).expect("parses");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].underlying, "NIFTY");
        assert_eq!(rows[0].contract_security_id, 4242);
        assert_eq!(rows[0].leg, "CE");
    }

    /// Expired contracts stay in the table forever (nothing evicts them), so
    /// without the expiry bound a rolled expiry would keep being subscribed and
    /// return silence indistinguishable from a quiet book.
    #[test]
    fn test_build_depth_candidate_query_bounds_by_expiry_and_refuses_zero_ids() {
        let sql = build_depth_candidate_query(1_780_000_000_000_000_000);
        assert!(
            sql.contains("expiry >="),
            "must exclude dead contracts: {sql}"
        );
        assert!(
            sql.contains("contract_security_id > 0"),
            "must exclude vendor-absent ids: {sql}"
        );
        assert!(
            sql.contains("LATEST ON ts"),
            "must collapse per-minute history to the newest row per contract: {sql}"
        );
        assert!(
            sql.contains("1780000000000000"),
            "nanos must be converted to micros for QuestDB: {sql}"
        );
    }

    /// The envelope is 5 conns x 50 for depth-20 and 5 x 1 for depth-200.
    /// Exceeding either makes `plan_pool` refuse the WHOLE lane, so a selection
    /// that overflows does not degrade — it takes the main feed down with it.
    #[test]
    fn test_selection_stays_inside_the_authorized_connection_envelope() {
        let mut rows = Vec::new();
        for (u, base) in [("NIFTY", 0_i64), ("BANKNIFTY", 1000), ("SENSEX", 2000)] {
            for i in 1..=200_i64 {
                for (leg_idx, leg) in ["CE", "PE"].iter().enumerate() {
                    let mut c = candidate(u, base + i * 2 + leg_idx as i64, i as f64, leg);
                    c.spot = 100.0;
                    rows.push(c);
                }
            }
        }
        let sel = select_depth_universe(&rows);
        assert!(
            sel.depth_20.len() <= 250,
            "depth-20 envelope is 5 conns x 50 = 250, got {}",
            sel.depth_20.len()
        );
        assert!(
            sel.depth_200.len() <= 5,
            "depth-200 envelope is 5 conns x 1 = 5, got {}",
            sel.depth_200.len()
        );
    }

    /// The day bound is what stops a pre-09:16 caller from silently receiving
    /// YESTERDAY's chain. `LATEST ON ts` returns the newest row per partition
    /// from ANY day without it.
    #[test]
    fn test_depth_candidate_query_is_day_bounded() {
        let today = 1_786_000_000_000_000_000i64;
        let sql = build_depth_candidate_query(today);
        let micros = today / 1_000;
        assert!(
            sql.contains(&format!("ts >= {micros}")),
            "the depth candidate query MUST bound ts to today — without it LATEST ON ts \
             returns yesterday's chain and depth ranks ATM off a stale spot. sql={sql}"
        );
        assert!(
            sql.contains(&format!("expiry >= {micros}")),
            "the expiry filter must survive alongside the day bound. sql={sql}"
        );
    }
}
