//! Bounded liveness check after the order stream's existing idle threshold.
//! RFC 6455 requires a Pong for Ping; the mirrored Dhan order-stream docs do
//! not promise a heartbeat cadence. A failed probe does not identify fault ownership.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use futures_util::Sink;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

use crate::sync::AtomicU64;
use crate::websocket::activity_watchdog::{
    ActivityWatchdog, WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS, spawn_with_panic_notify,
};

const SEND_TIMEOUT_SECS: u64 = 5;
const RESPONSE_TIMEOUT_SECS: u64 = 10;
pub(super) const SEND_TIMEOUT: Duration = Duration::from_secs(SEND_TIMEOUT_SECS);
pub(super) const RESPONSE_TIMEOUT: Duration = Duration::from_secs(RESPONSE_TIMEOUT_SECS);

/// Aborting a session must also stop its independent watchdog. Restarting
/// after successful liveness gives the socket a new full idle interval.
pub(super) struct SessionWatchdog {
    counter: Arc<AtomicU64>,
    notify: Arc<Notify>,
    task: JoinHandle<()>,
}

impl SessionWatchdog {
    pub(super) fn new(counter: Arc<AtomicU64>, notify: Arc<Notify>) -> Self {
        let task = Self::spawn(&counter, &notify);
        Self {
            counter,
            notify,
            task,
        }
    }

    fn spawn(counter: &Arc<AtomicU64>, notify: &Arc<Notify>) -> JoinHandle<()> {
        spawn_with_panic_notify(
            ActivityWatchdog::new(
                // O(1) EXEMPT: label allocation only when starting/rearming this cold idle watchdog.
                "order-update".to_string(),
                Arc::clone(counter),
                Duration::from_secs(WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS),
                Arc::clone(notify),
            )
            .with_liveness_probe(),
        )
    }

    pub(super) fn restart(&mut self) {
        self.task.abort();
        self.task = Self::spawn(&self.counter, &self.notify);
    }
}

impl Drop for SessionWatchdog {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) struct IdleProbe {
    nonce: [u8; 8],
    queued: bool,
    sending: bool,
    saw_activity: bool,
    deadline: Instant,
}

impl IdleProbe {
    pub(super) fn new(sequence: u64) -> Self {
        Self {
            nonce: sequence.to_be_bytes(),
            queued: false,
            sending: true,
            saw_activity: false,
            deadline: Instant::now() + SEND_TIMEOUT,
        }
    }

    pub(super) const fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(super) const fn sending(&self) -> bool {
        self.sending
    }

    pub(super) const fn timeout_phase(&self) -> (&'static str, u64) {
        if self.sending {
            ("sending Ping", SEND_TIMEOUT.as_secs())
        } else {
            (
                "awaiting Pong or inbound activity",
                RESPONSE_TIMEOUT.as_secs(),
            )
        }
    }

    /// Persist queue state across select cancellation: enqueue exactly one
    /// eight-byte Ping, then keep polling its flush while reads remain live.
    pub(super) fn poll_send<W: Sink<Message> + Unpin>(
        &mut self,
        writer: &mut W,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), W::Error>> {
        if !self.queued {
            ready!(Pin::new(&mut *writer).poll_ready(cx))?;
            Pin::new(&mut *writer).start_send(Message::Ping(self.nonce.to_vec().into()))?;
            self.queued = true;
        }
        Pin::new(writer).poll_flush(cx)
    }

    /// True means already received liveness while the Ping was being sent.
    pub(super) fn sent(&mut self) -> bool {
        self.sending = false;
        self.deadline = Instant::now() + RESPONSE_TIMEOUT;
        self.saw_activity
    }

    /// Any business/control input preserves the prior activity contract,
    /// except an unrelated Pong cannot answer this outstanding probe.
    /// Close and auth rejection are still handled by the original reader.
    pub(super) fn observe(&mut self, message: &Message) -> bool {
        let live = match message {
            Message::Pong(payload) => payload.as_ref() == self.nonce.as_slice(),
            Message::Close(_) => false,
            _ => true,
        };
        self.saw_activity |= live;
        live && !self.sending
    }
}

pub(super) async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_finished(handle: &tokio::task::AbortHandle) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while !handle.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("aborted watchdog must terminate");
    }

    async fn wait_released(counter: &Arc<AtomicU64>, notify: &Arc<Notify>) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while Arc::strong_count(counter) != 1 || Arc::strong_count(notify) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("aborted watchdog must release owned resources");
    }

    #[tokio::test(start_paused = true)]
    async fn test_idle_probe_nonce_and_deadline_do_not_follow_unrelated_pongs() {
        let mut probe = IdleProbe::new(7);
        assert!(!probe.sent());
        let deadline = probe.deadline();
        assert!(!probe.observe(&Message::Pong(6_u64.to_be_bytes().to_vec().into())));
        assert_eq!(probe.deadline(), deadline);
        assert!(probe.observe(&Message::Pong(7_u64.to_be_bytes().to_vec().into())));
    }

    #[tokio::test(start_paused = true)]
    async fn test_idle_probe_activity_during_send_keeps_original_write_deadline() {
        let mut probe = IdleProbe::new(1);
        let deadline = probe.deadline();
        assert!(!probe.observe(&Message::Text("activity".into())));
        assert_eq!(probe.deadline(), deadline);
        assert!(
            probe.sent(),
            "flushed Ping plus observed activity completes probe"
        );
    }

    #[tokio::test]
    async fn test_session_watchdog_drop_and_restart_abort_owned_tasks() {
        let counter = Arc::new(AtomicU64::new(0));
        let notify = Arc::new(Notify::new());
        let mut watchdog = SessionWatchdog::new(Arc::clone(&counter), Arc::clone(&notify));
        let first = watchdog.task.abort_handle();
        watchdog.restart();
        wait_finished(&first).await;
        assert!(first.is_finished(), "restart must stop the prior watchdog");
        let second = watchdog.task.abort_handle();
        drop(watchdog);
        wait_finished(&second).await;
        assert!(second.is_finished(), "drop must stop the current watchdog");
        assert_eq!(Arc::strong_count(&counter), 1);
        assert_eq!(Arc::strong_count(&notify), 1);
    }
    #[tokio::test]
    async fn test_session_watchdog_cancellation_aborts_child_task() {
        let counter = Arc::new(AtomicU64::new(0));
        let notify = Arc::new(Notify::new());
        let started = Arc::new(Notify::new());
        let task = tokio::spawn({
            let counter = Arc::clone(&counter);
            let notify = Arc::clone(&notify);
            let started = Arc::clone(&started);
            async move {
                let _watchdog = SessionWatchdog::new(counter, notify);
                started.notify_one();
                std::future::pending::<()>().await;
            }
        });
        tokio::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .expect("guard must be installed before cancellation");
        task.abort();
        assert!(task.await.expect_err("owner cancelled").is_cancelled());
        wait_released(&counter, &notify).await;
        assert_eq!(
            Arc::strong_count(&counter),
            1,
            "child must release its counter"
        );
        assert_eq!(
            Arc::strong_count(&notify),
            1,
            "child must release its notification"
        );
    }

    #[cfg(panic = "unwind")]
    #[tokio::test]
    async fn test_session_watchdog_unwind_aborts_child_task() {
        use futures_util::FutureExt;
        let counter = Arc::new(AtomicU64::new(0));
        let notify = Arc::new(Notify::new());
        let result = std::panic::AssertUnwindSafe(async {
            let _watchdog = SessionWatchdog::new(Arc::clone(&counter), Arc::clone(&notify));
            panic!("fixture session unwind");
        })
        .catch_unwind()
        .await;
        assert!(result.is_err());
        wait_released(&counter, &notify).await;
        assert_eq!(Arc::strong_count(&counter), 1);
        assert_eq!(Arc::strong_count(&notify), 1);
    }
}
