//! In-memory transports drive the real login/read lifecycle. No socket, TLS,
//! credential provider, recorder singleton or external service is involved.

#[cfg(test)]
mod tests {
    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;
    use std::task::{Context, Poll};

    use futures_util::FutureExt;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::sync::Notify;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::protocol::Role;

    use super::super::*;

    #[derive(Clone, Default)]
    struct SessionProbe {
        active: Arc<metrics::atomics::AtomicU64>,
        login_polled: Arc<AtomicBool>,
        read_entered: Arc<Notify>,
    }

    impl SessionProbe {
        fn active(&self) -> f64 {
            f64::from_bits(self.active.load(Ordering::SeqCst))
        }
    }

    #[derive(Clone, Copy)]
    enum WriteOutcome {
        Accept,
        Error,
        Pending,
        Panic,
    }

    #[derive(Clone, Copy)]
    enum ReadOutcome {
        Pending,
        Close,
        Error,
        Panic,
    }

    struct SessionIo {
        probe: SessionProbe,
        write: WriteOutcome,
        read: ReadOutcome,
    }

    impl AsyncRead for SessionIo {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            assert_eq!(
                self.probe.active(),
                1.0,
                "socket must report active during reads"
            );
            self.probe.read_entered.notify_one();
            match self.read {
                ReadOutcome::Pending => Poll::Pending,
                ReadOutcome::Close => {
                    // A real unmasked, empty WebSocket Close frame from the peer.
                    buffer.put_slice(&[0x88, 0x00]);
                    self.read = ReadOutcome::Pending;
                    Poll::Ready(Ok(()))
                }
                ReadOutcome::Error => Poll::Ready(Err(io::ErrorKind::ConnectionReset.into())),
                ReadOutcome::Panic => panic!("synthetic read unwind"),
            }
        }
    }

    impl AsyncWrite for SessionIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            assert_eq!(
                self.probe.active(),
                1.0,
                "connected socket must report active during login"
            );
            self.probe.login_polled.store(true, Ordering::SeqCst);
            match self.write {
                WriteOutcome::Accept => Poll::Ready(Ok(buffer.len())),
                WriteOutcome::Error => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
                WriteOutcome::Pending => Poll::Pending,
                WriteOutcome::Panic => panic!("synthetic login unwind"),
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    async fn run_session(
        probe: SessionProbe,
        write: WriteOutcome,
        read: ReadOutcome,
    ) -> Result<(), OrderUpdateConnectionError> {
        let active = metrics::Gauge::from_arc(Arc::clone(&probe.active));
        let stream = WebSocketStream::from_raw_socket(
            SessionIo { probe, write, read },
            Role::Client,
            Some(order_update_websocket_config()),
        )
        .await;
        let (orders, _receiver) = broadcast::channel(1);
        let mut stable = false;
        listen_on_connected_order_stream(
            stream,
            active,
            OrderUpdateSession {
                client_id: "fixture-client",
                access_token: "fixture-token",
                order_sender: &orders,
                wal_spill: None,
                authenticated_signal: None,
                authenticated_latch: None,
                notifier: None,
                failures_before_attempt: 0,
                stability_reached: &mut stable,
            },
        )
        .await
    }

    // Regression: 2026-10-31 — login send's early `?` left the active gauge at one.
    #[tokio::test]
    async fn test_regression_order_update_active_gauge_login_send_error() {
        let probe = SessionProbe::default();
        let result = run_session(probe.clone(), WriteOutcome::Error, ReadOutcome::Pending).await;
        assert!(matches!(result, Err(OrderUpdateConnectionError::Send(_))));
        assert!(probe.login_polled.load(Ordering::SeqCst));
        assert_eq!(
            probe.active(),
            0.0,
            "failed login must clear the active gauge"
        );
    }

    // Regression: 2026-10-31 — dropping a login that cannot finish left active=1.
    // This tests timeout cancellation, not a production login deadline: main does
    // not yet impose that deadline. A timeout's inner future is owned and dropped.
    #[tokio::test(start_paused = true)]
    async fn test_regression_order_update_active_gauge_login_timeout() {
        let probe = SessionProbe::default();
        let result = time::timeout(
            Duration::from_secs(1),
            run_session(probe.clone(), WriteOutcome::Pending, ReadOutcome::Pending),
        )
        .await;
        assert!(result.is_err());
        assert!(probe.login_polled.load(Ordering::SeqCst));
        assert_eq!(
            probe.active(),
            0.0,
            "timed-out login must clear the active gauge"
        );
    }

    // Regression: 2026-10-31 — aborting a connected read task bypassed manual resets.
    #[tokio::test]
    async fn test_regression_order_update_active_gauge_task_abort() {
        let probe = SessionProbe::default();
        let task = tokio::spawn(run_session(
            probe.clone(),
            WriteOutcome::Accept,
            ReadOutcome::Pending,
        ));
        time::timeout(Duration::from_secs(1), probe.read_entered.notified())
            .await
            .expect("session must reach its read loop");
        assert_eq!(probe.active(), 1.0);
        task.abort();
        assert!(
            task.await
                .expect_err("aborted session must be cancelled")
                .is_cancelled()
        );
        assert_eq!(
            probe.active(),
            0.0,
            "cancelled session must clear the active gauge"
        );
    }

    // Regression: 2026-10-31 — unwinding login/read paths bypassed manual resets.
    // Production panic=abort terminates the process and does not run destructors.
    #[cfg(panic = "unwind")]
    #[tokio::test]
    async fn test_regression_order_update_active_gauge_unwind() {
        for (write, read) in [
            (WriteOutcome::Panic, ReadOutcome::Pending),
            (WriteOutcome::Accept, ReadOutcome::Panic),
        ] {
            let probe = SessionProbe::default();
            let result = std::panic::AssertUnwindSafe(run_session(probe.clone(), write, read))
                .catch_unwind()
                .await;
            assert!(result.is_err(), "fixture must exercise unwinding");
            assert!(probe.login_polled.load(Ordering::SeqCst));
            assert_eq!(
                probe.active(),
                0.0,
                "unwound session must clear the active gauge"
            );
        }
    }

    // The outer reconnect loop awaits each session before starting the next one.
    // Reuse one gauge across clean closes, read failure, login failure and recovery.
    #[tokio::test]
    async fn test_regression_order_update_active_gauge_sequential_sessions() {
        let probe = SessionProbe::default();
        for (write, read, expected) in [
            (WriteOutcome::Accept, ReadOutcome::Close, "clean"),
            (WriteOutcome::Accept, ReadOutcome::Error, "read"),
            (WriteOutcome::Error, ReadOutcome::Pending, "login"),
            (WriteOutcome::Accept, ReadOutcome::Close, "clean"),
        ] {
            probe.login_polled.store(false, Ordering::SeqCst);
            let result = run_session(probe.clone(), write, read).await;
            match expected {
                "clean" => assert!(result.is_ok()),
                "read" => assert!(matches!(result, Err(OrderUpdateConnectionError::Read(_)))),
                "login" => assert!(matches!(result, Err(OrderUpdateConnectionError::Send(_)))),
                _ => unreachable!("fixed fixture cases"),
            }
            assert!(probe.login_polled.load(Ordering::SeqCst));
            assert_eq!(
                probe.active(),
                0.0,
                "previous session must be inactive before reconnect"
            );
        }
    }
}
