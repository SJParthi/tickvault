//! Audit B2 (2026-11-13) order-engine tests, moved out of `engine.rs` so the
//! file stays under its size pin (`source_size_ratchet_guard`). A child of
//! `engine.rs`'s `mod tests`, so its helpers are in scope.
//!
//! The body sits in a `#[cfg(test)]` block so the exit-order seam guard
//! (`dhan_exit_order_lockout_guard`, check 12) reads its exit-method calls as
//! test code, as it did inside `engine.rs`'s `mod tests`.

#[cfg(test)]
mod cases {
    use super::super::*;
    // ==== Audit B2 (2026-11-13): exit live arms take place_order's gates ====

    /// Finding 8: with no readiness snapshot installed, every exit method that
    /// sends a NEW order refuses in live mode before any token fetch, exactly
    /// as `place_order` does. Before the fix each arm went straight to the
    /// token fetch and the network call.
    #[tokio::test]
    async fn test_regression_exit_live_arms_refuse_without_readiness_before_token_fetch() {
        let (mut oms, calls, _rot) = make_live_oms_counting("http://127.0.0.1:1");
        let super_result = oms.place_super_order(make_super_request(), 1_800).await;
        assert!(
            matches!(super_result, Err(OmsError::OrderReadinessRefused { .. })),
            "super: {super_result:?}"
        );
        let forever_result = oms.place_forever_oco(make_forever_request()).await;
        assert!(
            matches!(forever_result, Err(OmsError::OrderReadinessRefused { .. })),
            "forever: {forever_result:?}"
        );
        let mut sliced = make_place_request();
        sliced.quantity = 250;
        let sliced_result = oms.place_order_sliced(sliced, 100).await;
        assert!(
            matches!(sliced_result, Err(OmsError::OrderReadinessRefused { .. })),
            "sliced: {sliced_result:?}"
        );
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            0,
            "no token may be fetched"
        );
        assert!(oms.all_orders().is_empty());
    }

    /// Finding 8: an engaged order-path halt (DH-902/903 class) refuses the
    /// exit arms too, even with readiness green.
    #[tokio::test]
    async fn test_regression_exit_live_arms_refuse_while_order_path_halted() {
        let (mut oms, calls, _rot) = make_live_oms_counting("http://127.0.0.1:1");
        install_ready(&mut oms);
        assert!(oms.engage_halt("DH-903", "test halt".to_owned()));
        let super_result = oms.place_super_order(make_super_request(), 1_800).await;
        assert!(matches!(
            super_result,
            Err(OmsError::OrderPathHalted { .. })
        ));
        let forever_result = oms.place_forever_oco(make_forever_request()).await;
        assert!(matches!(
            forever_result,
            Err(OmsError::OrderPathHalted { .. })
        ));
        let mut sliced = make_place_request();
        sliced.quantity = 250;
        let sliced_result = oms.place_order_sliced(sliced, 100).await;
        assert!(matches!(
            sliced_result,
            Err(OmsError::OrderPathHalted { .. })
        ));
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
    }

    /// Finding 8 (source pin): the sandbox sentinel sits in the shared exit
    /// gate, and each new-order exit arm calls that gate between its
    /// `// LIVE-EXIT-ARM` marker and its token fetch. The sentinel is
    /// compiled out of tests, so only a source pin can prove it is there.
    #[test]
    fn test_exit_live_arms_call_the_shared_gate_before_the_token_fetch() {
        let src = include_str!("../../engine.rs");
        let production = src
            .split(concat!("#[cfg(", "test)]\nmod tests"))
            .next()
            .unwrap_or(src);
        let gate_start = production
            .find("fn check_live_exit_gates(")
            .expect("shared exit gate exists");
        let gate_len = production[gate_start..]
            .find("\n    \u{7d}\n")
            .expect("gate body end");
        let exit_gate_body = &production[gate_start..gate_start + gate_len];
        let sandbox_call = exit_gate_body
            .find("self.refuse_inside_sandbox()?")
            .expect("the exit gate runs the sandbox sentinel");
        let order_gate = exit_gate_body
            .find("self.check_live_order_gates(endpoint")
            .expect("then halt and readiness");
        assert!(sandbox_call < order_gate);
        let sandbox_start = production
            .find("fn refuse_inside_sandbox(")
            .expect("shared sandbox sentinel exists");
        let sandbox_len = production[sandbox_start..]
            .find("\n    \u{7d}\n")
            .expect("sandbox body end");
        let gate_body = &production[sandbox_start..sandbox_start + sandbox_len];
        assert!(gate_body.contains("#[cfg(not(test))]"));
        assert!(gate_body.contains("now_secs < SANDBOX_DEADLINE_EPOCH_SECS"));
        assert!(gate_body.contains("OmsError::SandboxEnforcement"));
        // The sandbox refusal gives back the HalfOpen probe and the order
        // budget (nothing was sent; B2 review 2026-11-13).
        let sandbox_return = gate_body
            .find("return Err(OmsError::SandboxEnforcement)")
            .expect("sandbox return");
        let release = gate_body
            .find("self.release_unsent_order()")
            .expect("probe and budget release");
        assert!(release < sandbox_return, "probe released before the return");
        // The same in place_order's own sandbox branch.
        let place = production
            .find("pub async fn place_order(&mut self")
            .expect("place_order");
        let place_body = &production[place..];
        let sentinel = place_body
            .find("now_secs < SANDBOX_DEADLINE_EPOCH_SECS")
            .expect("place_order sandbox gate");
        let place_branch = &place_body[sentinel..];
        let place_return = place_branch
            .find("return Err(OmsError::SandboxEnforcement)")
            .expect("place_order sandbox return");
        assert!(
            place_branch[..place_return].contains("self.release_unsent_order()"),
            "place_order's sandbox refusal must give back the HalfOpen probe and the budget"
        );
        for method in [
            "place_super_order",
            "place_forever_oco",
            "place_order_sliced",
            "modify_super_order_leg",
        ] {
            let start = production
                .find(&format!("pub async fn {method}("))
                .expect("every exit method named in the list exists");
            let body = &production[start..];
            let marker = body.find("// LIVE-EXIT-ARM").expect("marker");
            let gate = body.find("self.check_live_exit_gates(").expect("gate call");
            let token = body.find("get_access_token()").expect("token fetch");
            assert!(
                marker < gate && gate < token,
                "{method}: marker@{marker} gate@{gate} token@{token}"
            );
        }
    }

    /// Finding 9: clearing an order-path halt must not wipe today's order
    /// budget counters.
    #[test]
    fn test_regression_clear_order_halt_keeps_the_order_budget() {
        let mut oms = make_dry_oms();
        let now = now_epoch_s();
        for _ in 0..5 {
            oms.order_budget.try_consume(now).expect("budget room");
        }
        assert_eq!(oms.order_budget.used_today(), 5);
        assert!(oms.engage_halt("DH-903", "test halt".to_owned()));
        oms.clear_order_halt();
        assert!(!oms.is_order_halted());
        assert_eq!(
            oms.order_budget.used_today(),
            5,
            "clearing a halt must keep the day's order count"
        );
    }

    /// B2 review (2026-11-13): an order refused by the halt gate sends
    /// nothing, so it spends no order budget. Before, the budget was spent
    /// before the gate and nothing gave it back, and since the halt clear no
    /// longer resets the budget, refusals during a halt used it up: after
    /// the clear, every place, modify and CANCEL was refused with
    /// `OrderBudgetExhausted` until the window rolled. Here the minute tier
    /// is one short of full, one place is refused by the halt, and the
    /// cancel after the clear must still go out.
    #[tokio::test]
    async fn test_regression_halt_refused_orders_spend_no_budget() {
        let body = r#"{"orderId":"1","orderStatus":"CANCELLED"}"#;
        let (base_url, handle) = start_oms_mock(200, body).await;
        let mut oms = make_live_oms(&base_url);
        install_ready(&mut oms);
        insert_open_order(&mut oms, "1");
        let now = now_epoch_s();
        for _ in 0..(OrderBudget::MAX_PER_MINUTE - 1) {
            oms.order_budget.try_consume(now).expect("budget room");
        }
        let before = oms.order_budget.used_today();
        assert!(oms.engage_halt("DH-903", "test halt".to_owned()));
        let refused = oms.place_order(make_place_request()).await;
        assert!(matches!(
            refused,
            Err(OmsError::OrderPathHalted { cause: "DH-903" })
        ));
        assert_eq!(
            oms.order_budget.used_today(),
            before,
            "a halt-refused order must give its budget back"
        );
        oms.clear_order_halt();
        let cancel = oms.cancel_order("1").await;
        assert!(
            cancel.is_ok(),
            "the cancel after the clear must not be refused by the budget: {cancel:?}"
        );
        handle.abort();
    }

    /// B2 review (2026-11-13): a readiness refusal (no snapshot installed)
    /// also gives the budget back, on `modify_order` as on `place_order`.
    #[tokio::test]
    async fn test_regression_readiness_refused_modify_spends_no_budget() {
        let (mut oms, calls, _rot) = make_live_oms_counting("http://127.0.0.1:1");
        insert_open_order(&mut oms, "1");
        let before = oms.order_budget.used_today();
        let modify = ModifyOrderRequest {
            order_type: OrderType::Limit,
            quantity: 50,
            price: 246.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };
        assert!(oms.modify_order("1", modify).await.is_err());
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0, "nothing was sent");
        assert_eq!(oms.order_budget.used_today(), before);
    }

    /// B2 review (2026-11-13): a circuit-breaker refusal on the cancel and
    /// place paths sends nothing and spends no budget.
    #[tokio::test]
    async fn test_regression_circuit_refused_orders_spend_no_budget() {
        let mut oms = make_dry_oms();
        insert_open_order(&mut oms, "1");
        for _ in 0..tickvault_common::constants::OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD {
            oms.circuit_breaker.record_failure();
        }
        let before = oms.order_budget.used_today();
        assert!(matches!(
            oms.cancel_order("1").await,
            Err(OmsError::CircuitBreakerOpen)
        ));
        assert!(matches!(
            oms.place_order(make_place_request()).await,
            Err(OmsError::CircuitBreakerOpen)
        ));
        assert_eq!(oms.order_budget.used_today(), before);
    }

    /// Audit B2 follow-up (2026-11-13): the 16:00 IST daily reset does NOT
    /// reopen the day tier. The day tier is the IST calendar date (finding 9
    /// is met by the date rolling at 00:00 IST); clearing it at 16:00 let a
    /// day's 7,000 be spent twice inside one IST date.
    #[test]
    fn test_regression_reset_daily_keeps_the_day_tier_of_the_ist_date() {
        let mut oms = make_dry_oms();
        // 2026-11-13 15:59:00 IST.
        let t = 1_794_565_740_u64;
        for _ in 0..5 {
            oms.order_budget.try_consume(t).expect("budget room");
        }
        oms.reset_daily();
        assert_eq!(
            oms.order_budget.used_today(),
            5,
            "the 16:00 reset keeps the IST date's count"
        );
        // 2026-11-14 00:00:00 IST: a new date, a new day window.
        oms.order_budget
            .try_consume(1_794_594_600)
            .expect("new IST date");
        assert_eq!(oms.order_budget.used_today(), 1);
    }

    /// Audit B2 (2026-11-13): the daily reset leaves the minute tier to roll
    /// by time. A burst that fills the minute just before the 16:00 reset must
    /// stay refused for the rest of that minute, or the broker's 250/min is
    /// exceeded inside one rolling window.
    #[test]
    fn test_regression_reset_daily_keeps_a_full_minute_tier() {
        let mut oms = make_dry_oms();
        let t = 1_000_000_u64;
        for _ in 0..OrderBudget::MAX_PER_MINUTE {
            oms.order_budget.try_consume(t).expect("fills the minute");
        }
        oms.reset_daily();
        assert!(matches!(
            oms.order_budget.try_consume(t + 5),
            Err(OmsError::OrderBudgetExhausted { tier: "minute", .. })
        ));
        oms.order_budget
            .try_consume(t + 60)
            .expect("the minute rolls by time");
    }

    /// Audit B2 (2026-11-13): a leg modify on a pending super order takes the
    /// same live gates as `modify_order`. With no readiness, and while the
    /// order path is halted, it refuses before any token fetch.
    #[tokio::test]
    async fn test_regression_super_leg_modify_takes_the_live_gates() {
        let (mut oms, calls, _rot) = make_live_oms_counting("http://127.0.0.1:1");
        insert_tracked_super(&mut oms, "SO-9", "PENDING", 75);
        let stop = || ModifySuperOrderLeg::StopLoss {
            stop_loss_price: 96.0,
            trailing_jump: 0.0,
        };
        let refused = oms.modify_super_order_leg("SO-9", stop()).await;
        assert!(
            matches!(refused, Err(OmsError::OrderReadinessRefused { .. })),
            "no readiness: {refused:?}"
        );
        install_ready(&mut oms);
        assert!(oms.engage_halt("DH-903", "test halt".to_owned()));
        let halted = oms.modify_super_order_leg("SO-9", stop()).await;
        assert!(
            matches!(halted, Err(OmsError::OrderPathHalted { .. })),
            "halted: {halted:?}"
        );
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0, "no token fetched");
    }

    /// Audit B2 (2026-11-13): a live gate that refuses after the circuit
    /// breaker gave out its single HalfOpen probe gives the probe back, so
    /// the next order is not refused with CircuitBreakerOpen until the daily
    /// reset. Covers an exit arm and `place_order`.
    #[tokio::test]
    async fn test_regression_live_gate_refusal_gives_back_the_half_open_probe() {
        let (mut oms, calls, _rot) = make_live_oms_counting("http://127.0.0.1:1");
        oms.circuit_breaker.force_half_open_for_test();
        let refused = oms.place_super_order(make_super_request(), 1_800).await;
        assert!(
            matches!(refused, Err(OmsError::OrderReadinessRefused { .. })),
            "super: {refused:?}"
        );
        let refused = oms.place_order(make_place_request()).await;
        assert!(
            matches!(refused, Err(OmsError::OrderReadinessRefused { .. })),
            "place_order after a refused exit arm: {refused:?}"
        );
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
        install_ready(&mut oms);
        let sent = oms.place_super_order(make_super_request(), 1_800).await;
        assert!(
            !matches!(sent, Err(OmsError::CircuitBreakerOpen)),
            "the probe must be free once readiness is green: {sent:?}"
        );
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            1,
            "the probe request reached the token fetch"
        );
    }

    // -----------------------------------------------------------------------
    // Audit B2 follow-up (2026-11-13): every exit after the breaker's check
    // that records no outcome gives the HalfOpen probe back.
    // -----------------------------------------------------------------------

    /// Token provider whose token is always missing.
    struct NoTokenProvider;
    impl TokenProvider for NoTokenProvider {
        fn get_access_token(&self) -> Result<SecretString, OmsError> {
            Err(OmsError::NoToken)
        }
        fn request_token_rotation(&self) -> bool {
            false
        }
    }

    fn make_live_oms_without_token() -> OrderManagementSystem {
        let http = reqwest::Client::builder().build().unwrap();
        let api_client =
            OrderApiClient::new(http, "http://127.0.0.1:1".to_owned(), "100".to_owned());
        let mut oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(NoTokenProvider),
            "100".to_owned(),
        );
        oms.enable_live_mode();
        install_ready(&mut oms);
        oms
    }

    /// A failed token fetch after the probe was taken sends nothing, so the
    /// next order must be refused for its own reason, never for a probe
    /// "already in flight". Covers place, modify, cancel and an exit arm.
    #[tokio::test]
    async fn test_regression_failed_token_fetch_gives_back_the_half_open_probe() {
        let mut oms = make_live_oms_without_token();
        insert_open_order(&mut oms, "1");
        oms.circuit_breaker.force_half_open_for_test();
        let first = oms.place_order(make_place_request()).await;
        assert!(matches!(first, Err(OmsError::NoToken)), "place: {first:?}");
        let modify = ModifyOrderRequest {
            order_type: OrderType::Limit,
            quantity: 50,
            price: 246.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        };
        let second = oms.modify_order("1", modify).await;
        assert!(
            matches!(second, Err(OmsError::NoToken)),
            "modify: {second:?}"
        );
        let third = oms.cancel_order("1").await;
        assert!(matches!(third, Err(OmsError::NoToken)), "cancel: {third:?}");
        let fourth = oms.place_super_order(make_super_request(), 1_800).await;
        assert!(
            matches!(fourth, Err(OmsError::NoToken)),
            "super: {fourth:?}"
        );
        let fifth = oms.place_order(make_place_request()).await;
        assert!(
            matches!(fifth, Err(OmsError::NoToken)),
            "place again: {fifth:?}"
        );
    }

    fn sample_modify_request() -> ModifyOrderRequest {
        ModifyOrderRequest {
            order_type: OrderType::Limit,
            quantity: 50,
            price: 246.0,
            trigger_price: 0.0,
            validity: OrderValidity::Day,
            disclosed_quantity: 0,
        }
    }

    /// B2 review round 4 (2026-11-13): a failed token fetch before any send
    /// gives the order's budget back on place, modify, cancel, an exit arm
    /// and verify. Before, each attempt while the token was missing kept one
    /// order spent, so retries could fill the hour tier and lock out cancels.
    #[tokio::test]
    async fn test_regression_failed_token_fetch_spends_no_budget() {
        let mut oms = make_live_oms_without_token();
        insert_open_order(&mut oms, "1");
        let before = oms.order_budget.used_today();
        let place = oms.place_order(make_place_request()).await;
        assert!(matches!(place, Err(OmsError::NoToken)), "place: {place:?}");
        assert_eq!(oms.order_budget.used_today(), before, "place");
        let modify = oms.modify_order("1", sample_modify_request()).await;
        assert!(matches!(modify, Err(OmsError::NoToken)), "{modify:?}");
        assert_eq!(oms.order_budget.used_today(), before, "modify");
        let cancel = oms.cancel_order("1").await;
        assert!(matches!(cancel, Err(OmsError::NoToken)), "{cancel:?}");
        assert_eq!(oms.order_budget.used_today(), before, "cancel");
        let exit = oms.place_super_order(make_super_request(), 1_800).await;
        assert!(matches!(exit, Err(OmsError::NoToken)), "super: {exit:?}");
        assert_eq!(oms.order_budget.used_today(), before, "exit arm");
        let verify = oms.verify_order_execution("1", 0, 60).await;
        assert!(matches!(verify, Err(OmsError::NoToken)), "{verify:?}");
        assert_eq!(oms.order_budget.used_today(), before, "verify");
    }

    /// Token provider that hands out a token until the first rotation, then
    /// has none: the auth retry pass of a loop fails its token fetch.
    struct TokenGoneAfterRotation {
        rotated: Arc<std::sync::atomic::AtomicBool>,
    }
    impl TokenProvider for TokenGoneAfterRotation {
        fn get_access_token(&self) -> Result<SecretString, OmsError> {
            if self.rotated.load(AtomicOrdering::SeqCst) {
                Err(OmsError::NoToken)
            } else {
                Ok(SecretString::from("test-token"))
            }
        }
        fn request_token_rotation(&self) -> bool {
            self.rotated.store(true, AtomicOrdering::SeqCst);
            true
        }
    }

    /// B2 review round 4: after an auth rotate-retry the first request WAS
    /// sent, so a token fetch that fails on the retry pass keeps that order
    /// counted (never under-counts what reached the broker).
    #[tokio::test(start_paused = true)]
    async fn test_token_fetch_failing_on_the_auth_retry_pass_keeps_the_budget() {
        let body = r#"{"errorType":"INVALID_AUTHENTICATION","errorCode":"DH-901","errorMessage":"invalid"}"#;
        let (base_url, handle) = start_multi_mock(vec![(401u16, body.to_owned())]).await;
        let http = reqwest::Client::builder().build().unwrap();
        let api_client = OrderApiClient::new(http, base_url, "100".to_owned());
        let mut oms = OrderManagementSystem::new(
            api_client,
            OrderRateLimiter::new(10),
            Box::new(TokenGoneAfterRotation {
                rotated: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }),
            "100".to_owned(),
        );
        oms.enable_live_mode();
        install_ready(&mut oms);
        let before = oms.order_budget.used_today();
        let place = oms.place_order(make_place_request()).await;
        assert!(matches!(place, Err(OmsError::NoToken)), "{place:?}");
        assert_eq!(
            oms.order_budget.used_today(),
            before + 1,
            "the first request reached the broker and stays counted"
        );
        handle.abort();
    }

    /// B2 review round 4 (2026-11-13): while the DATA-805 STOP-ALL latch
    /// holds, place, modify and cancel are refused before any send and give
    /// their budget back; the stop-all alert still fires once per episode.
    #[tokio::test]
    async fn test_regression_stop_all_latch_refusals_spend_no_budget() {
        let body = r#"{"errorType":"x","errorCode":805,"errorMessage":"too many"}"#;
        let (base_url, handle) = start_oms_mock(400, body).await;
        let mut oms = make_live_oms(&base_url);
        install_ready(&mut oms);
        insert_open_order(&mut oms, "1");
        let sink = RecordingSink::default();
        oms.set_alert_sink(Box::new(sink.clone()));
        let first = oms.place_order(make_place_request()).await.unwrap_err();
        assert!(matches!(first, OmsError::DhanApiError { .. }), "{first:?}");
        let before = oms.order_budget.used_today();
        let place = oms.place_order(make_place_request()).await;
        assert!(
            matches!(place, Err(OmsError::StopAllCooldown { .. })),
            "{place:?}"
        );
        assert_eq!(oms.order_budget.used_today(), before, "place");
        let modify = oms.modify_order("1", sample_modify_request()).await;
        assert!(
            matches!(modify, Err(OmsError::StopAllCooldown { .. })),
            "{modify:?}"
        );
        assert_eq!(oms.order_budget.used_today(), before, "modify");
        let cancel = oms.cancel_order("1").await;
        assert!(
            matches!(cancel, Err(OmsError::StopAllCooldown { .. })),
            "{cancel:?}"
        );
        assert_eq!(oms.order_budget.used_today(), before, "cancel");
        assert_eq!(
            sink.count(|a| matches!(a, OmsAlert::OrderApiStopAll { .. })),
            1,
            "C2: one stop-all alert per episode"
        );
        handle.abort();
    }

    /// A throttled answer (HTTP 429) to the probe on an exit arm records no
    /// breaker outcome; the probe must come back for the next order.
    #[tokio::test]
    async fn test_regression_rate_limited_exit_probe_gives_back_the_half_open_probe() {
        let body = r#"{"errorType":"RATE_LIMIT","errorCode":"DH-904","errorMessage":"throttled"}"#;
        let responses = vec![(429u16, body.to_owned()); 2];
        let (base_url, handle) = start_multi_mock(responses).await;
        let mut oms = make_live_oms(&base_url);
        install_ready(&mut oms);
        oms.circuit_breaker.force_half_open_for_test();
        let first = oms.place_super_order(make_super_request(), 1_800).await;
        assert!(matches!(first, Err(OmsError::DhanRateLimited)), "{first:?}");
        let second = oms.place_super_order(make_super_request(), 1_800).await;
        assert!(
            matches!(second, Err(OmsError::DhanRateLimited)),
            "the second order must reach the broker, not CircuitBreakerOpen: {second:?}"
        );
        handle.abort();
    }

    /// A DH-904 ladder exhaustion answering the probe on `place_order` (a
    /// class the breaker does not count) gives the probe back.
    #[tokio::test(start_paused = true)]
    async fn test_regression_rate_limited_place_probe_gives_back_the_half_open_probe() {
        let body = r#"{"errorType":"RATE_LIMIT","errorCode":"DH-904","errorMessage":"throttled"}"#;
        let responses = vec![(429u16, body.to_owned()); 10];
        let (base_url, handle) = start_multi_mock(responses).await;
        let mut oms = make_live_oms_no_timeout(&base_url);
        install_ready(&mut oms);
        oms.circuit_breaker.force_half_open_for_test();
        let first = oms.place_order(make_place_request()).await;
        assert!(matches!(first, Err(OmsError::DhanRateLimited)), "{first:?}");
        let second = oms.place_order(make_place_request()).await;
        assert!(
            matches!(second, Err(OmsError::DhanRateLimited)),
            "the second order must reach the broker, not CircuitBreakerOpen: {second:?}"
        );
        handle.abort();
    }

    /// Every token fetch on a path that took the breaker probe goes through
    /// `probe_back_on_token_err`; only `reconcile` and
    /// `verify_order_execution`, which never take the probe, fetch the token
    /// bare. And `modify_order` runs the sandbox sentinel, then halt and
    /// readiness, before its token fetch.
    ///
    /// B2 review round 4 (2026-11-13): the three loops pass
    /// `!auth_retry_used` (budget back only before the first send), the five
    /// exit arms pass `true`, `verify_order_execution` gives its budget back
    /// through `budget_back_on_token_err`, and the three loops check the
    /// STOP-ALL latch before their first send. Whitespace is ignored, so a
    /// rustfmt reflow cannot hide a call.
    #[test]
    fn test_token_fetches_after_the_breaker_check_go_through_the_probe_release() {
        let src = include_str!("../../engine.rs");
        let production = src
            .split(concat!("#[cfg(", "test)]\nmod tests"))
            .next()
            .unwrap_or(src);
        let squashed: String = production.chars().filter(|c| !c.is_whitespace()).collect();
        let fetches = squashed
            .matches("self.token_provider.get_access_token()")
            .count();
        let looped = squashed
            .matches(
                "self.probe_back_on_token_err(self.token_provider.get_access_token(),!auth_retry_used",
            )
            .count();
        let exits = squashed
            .matches("self.probe_back_on_token_err(self.token_provider.get_access_token(),true)?")
            .count();
        let verify = squashed
            .matches("self.budget_back_on_token_err(self.token_provider.get_access_token())?")
            .count();
        assert_eq!(looped, 3, "place, modify and cancel");
        assert_eq!(exits, 5, "the five exit arms");
        assert_eq!(verify, 1, "verify_order_execution");
        assert_eq!(
            fetches - looped - exits - verify,
            1,
            "only reconcile fetches the token bare"
        );
        assert_eq!(
            squashed
                .matches("self.refuse_unsent_inside_stop_all()?;letmutauth_retry_used=false;")
                .count(),
            3,
            "place, modify and cancel check the STOP-ALL latch before the first send"
        );
        for method in [
            "pub async fn reconcile(",
            "pub async fn verify_order_execution(",
        ] {
            let start = production.find(method).expect("method exists");
            let body = &production[start..];
            let end = body.find("\n    \u{7d}\n").expect("body end");
            assert!(
                !body[..end].contains("circuit_breaker.check()")
                    && !body[..end].contains("self.check_order_gates()"),
                "{method} takes no probe"
            );
            assert!(body[..end].contains("self.token_provider.get_access_token()"));
        }
        let modify = production
            .find("pub async fn modify_order(")
            .expect("modify_order");
        let body = &production[modify..];
        let sandbox = body
            .find("self.refuse_inside_sandbox()?")
            .expect("modify_order runs the sandbox sentinel");
        let gate = body
            .find("self.check_live_order_gates(OrderEndpoint::Modify, order_id)?")
            .expect("then halt and readiness");
        let token = body.find("get_access_token()").expect("token fetch");
        assert!(sandbox < gate && gate < token);
        let dry = body.find("if self.dry_run").expect("dry-run branch");
        assert!(dry < sandbox, "the paper branch still returns first");
    }
}
