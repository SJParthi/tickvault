//! Effective kernel receive-buffer sizes for established Dhan feed sockets.
//! A duplicate descriptor lets the metrics worker sample after the WebSocket
//! has split into reader and writer. It never reads bytes or sets options.
//! `tv_dhan_ws_receive_buffer_bytes{connection}` is the Linux `SO_RCVBUF`
//! readback, including kernel accounting overhead. An unavailable or closed
//! slot reports -1. The metrics worker refreshes established slots every 30 s.

use std::sync::Mutex;

use metrics::Gauge;
use tokio::net::{TcpSocket, TcpStream};

use super::pool_budget::{MAX_TOTAL_DHAN_CONNECTIONS, connection_slot_label};

const METRIC: &str = "tv_dhan_ws_receive_buffer_bytes";
const UNAVAILABLE: f64 = -1.0;

struct Probe {
    socket: TcpSocket,
    gauge: Gauge,
}

impl Probe {
    fn sample(&self) {
        let value = match self.socket.recv_buffer_size() {
            Ok(bytes) if bytes > 0 => f64::from(bytes),
            _ => UNAVAILABLE,
        };
        self.gauge.set(value);
    }
}

struct Slot {
    generation: u64,
    probe: Option<Probe>,
}

const EMPTY: Slot = Slot {
    generation: 0,
    probe: None,
};
static SLOTS: [Mutex<Slot>; MAX_TOTAL_DHAN_CONNECTIONS as usize] =
    [const { Mutex::new(EMPTY) }; MAX_TOTAL_DHAN_CONNECTIONS as usize];

#[cfg(target_os = "linux")]
fn observation_socket(stream: &TcpStream) -> Option<TcpSocket> {
    use std::os::fd::AsFd;
    let descriptor = stream.as_fd().try_clone_to_owned().ok()?;
    Some(TcpSocket::from_std_stream(descriptor.into()))
}

#[cfg(not(target_os = "linux"))]
fn observation_socket(_: &TcpStream) -> Option<TcpSocket> {
    None
}

/// Owned by the feed session, including while it is being subscribed.
pub(super) struct Registration {
    slot: &'static Mutex<Slot>,
    generation: u64,
}

impl Registration {
    pub(super) fn attach(global_index: u8, stream: &TcpStream) -> Option<Self> {
        let slot = SLOTS.get(usize::from(global_index))?;
        let gauge = metrics::gauge!(METRIC, "connection" => connection_slot_label(global_index));
        Self::replace(slot, observation_socket(stream), gauge)
    }

    fn replace(
        slot: &'static Mutex<Slot>,
        socket: Option<TcpSocket>,
        gauge: Gauge,
    ) -> Option<Self> {
        let mut state = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.generation = state.generation.wrapping_add(1);
        let Some(socket) = socket else {
            state.probe = None;
            gauge.set(UNAVAILABLE);
            return None;
        };
        let probe = Probe { socket, gauge };
        probe.sample();
        state.probe = Some(probe);
        Some(Self {
            slot,
            generation: state.generation,
        })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut state = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Teardown from an older connection may race a replacement in this slot.
        if state.generation == self.generation
            && let Some(probe) = state.probe.take()
        {
            probe.gauge.set(UNAVAILABLE);
        }
    }
}

/// Called by the 30-second metrics thread. Each live slot costs one getsockopt.
/// Holding the slot lock through publication prevents a delayed sample from
/// overwriting a disconnect or a newer session's first observation.
pub fn refresh() {
    for slot in &SLOTS {
        let state = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(probe) = &state.probe {
            probe.sample();
        }
    }
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;
    use crate::websocket::connection::{DhanFeedSocketImpl, DhanSocketParams, FeedTokenBuffer};
    use crate::websocket::pool_budget::DhanEndpointType;
    use crate::websocket::pool_supervisor::DhanFeedSocket;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn reports_the_effective_setting_without_pinning_a_closed_connection() {
        const INDEX: usize = MAX_TOTAL_DHAN_CONNECTIONS as usize - 2;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        let control = observation_socket(&client).unwrap();
        let value = Arc::new(AtomicU64::new(0));
        let gauge = Gauge::from_arc(Arc::clone(&value));
        let read = || f64::from_bits(value.load(Ordering::Relaxed));
        let initial = control.recv_buffer_size().unwrap();
        let old = Registration::replace(&SLOTS[INDEX], observation_socket(&client), gauge.clone())
            .unwrap();
        assert_eq!(read(), f64::from(initial));
        assert_eq!(control.recv_buffer_size().unwrap(), initial);
        // Only this test sets a value, to verify that a later sample sees a change.
        control.set_recv_buffer_size(8192).unwrap();
        let expected = control.recv_buffer_size().unwrap();
        refresh();
        assert_eq!(read(), f64::from(expected));
        assert!(expected > 0);

        let replacement = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (_next_peer, _) = listener.accept().await.unwrap();
        let replacement_size = observation_socket(&replacement)
            .unwrap()
            .recv_buffer_size()
            .unwrap();
        let new = Registration::replace(
            &SLOTS[INDEX],
            observation_socket(&replacement),
            gauge.clone(),
        )
        .unwrap();
        drop(old);
        drop(control);
        drop(client);
        let mut byte = [0u8; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), peer.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        refresh();
        assert_eq!(read(), f64::from(replacement_size));
        drop(new);
        refresh();
        assert_eq!(read(), UNAVAILABLE);

        let stale = Registration::replace(
            &SLOTS[INDEX],
            observation_socket(&replacement),
            gauge.clone(),
        )
        .unwrap();
        assert!(Registration::replace(&SLOTS[INDEX], None, gauge).is_none());
        drop(stale);
        refresh();
        assert_eq!(read(), UNAVAILABLE);
        assert!(Registration::attach(MAX_TOTAL_DHAN_CONNECTIONS, &replacement).is_none());
    }

    #[tokio::test]
    async fn feed_dial_close_reconnect_and_drop_release_the_observation() {
        const INDEX: u8 = MAX_TOTAL_DHAN_CONNECTIONS - 1;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                if matches!(
                    ws.next().await,
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))
                ) {
                    ws.flush().await.unwrap();
                }
            }
        });
        let _already_installed = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut socket = DhanFeedSocketImpl::new(
            DhanSocketParams::new(
                DhanEndpointType::MainFeed,
                format!("ws://{address}"),
                "fixture".into(),
            ),
            || Some(FeedTokenBuffer::new("fixture".into())),
        )
        .with_receive_buffer_slot(INDEX);
        let reading = || {
            SLOTS[usize::from(INDEX)]
                .lock()
                .unwrap()
                .probe
                .as_ref()
                .map(|probe| probe.socket.recv_buffer_size().unwrap())
        };
        socket.connect().await.unwrap();
        assert!(reading().is_some_and(|bytes| bytes > 0));
        socket.close(|_| {}).await;
        assert_eq!(reading(), None);
        socket.connect().await.unwrap();
        assert!(reading().is_some_and(|bytes| bytes > 0));
        drop(socket);
        assert_eq!(reading(), None);
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
    }
}
