//! The one TCP dial every Dhan WebSocket goes through, with kernel keepalive on.
//!
//! WHY THIS EXISTS (2026-11-13, audit B2 finding 7)
//! ================================================
//! `deploy/aws/sysctl/99-tickvault-net.conf` tunes the kernel keepalive ladder
//! (`tcp_keepalive_time`, `_intvl`, `_probes`) as the last backstop for a
//! half-open link, and `host_tuning` checks those values at boot. Linux
//! applies them ONLY to a socket that has `SO_KEEPALIVE` set, and
//! `tokio_tungstenite::connect_async_tls_with_config` dials with a plain
//! `TcpStream::connect`, which never sets it. So the backstop did nothing on
//! any Dhan socket. On the order-update socket the app's own watchdog waits
//! 4 hours, so a half-open link there could lose order updates for hours.
//!
//! This module does the TCP part of the dial itself: it resolves the host the
//! same way the library does, opens each candidate address with a
//! `tokio::net::TcpSocket` that has `SO_KEEPALIVE` (and `TCP_NODELAY` when
//! asked) set BEFORE the connect, then hands the connected stream to
//! `tokio_tungstenite::client_async_tls_with_config` for the TLS handshake and
//! the HTTP upgrade. The TLS connector, the WebSocket config and the read path
//! are unchanged: the caller passes exactly what it passed before.
//!
//! Only the switch is set here. The timings (first probe, interval, count)
//! come from the sysctl file, so the box keeps one place that defines them.
//!
//! Cold path: runs once per dial, never per frame.

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::{TcpSocket, TcpStream};
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::error::UrlError;
use tokio_tungstenite::tungstenite::handshake::client::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

/// The connected, upgraded socket every dial returns.
pub type DialedWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Port for a request: the URL's own port, else 443 for `wss` and 80 for `ws`.
/// Same rule as the library's own dial.
fn port_of(request: &Request) -> Result<u16, WsError> {
    request
        .uri()
        .port_u16()
        .or_else(|| match request.uri().scheme_str() {
            Some("wss") => Some(443),
            Some("ws") => Some(80),
            _ => None,
        })
        .ok_or(WsError::Url(UrlError::UnsupportedUrlScheme))
}

/// Host for a request, with IPv6 brackets removed so it resolves.
fn host_of(request: &Request) -> Result<&str, WsError> {
    let host = request
        .uri()
        .host()
        .ok_or(WsError::Url(UrlError::NoHostName))?;
    Ok(host.trim_start_matches('[').trim_end_matches(']'))
}

/// How long one resolved address may take to complete its TCP connect before
/// the dial moves to the next address (B2 review, 2026-11-13).
///
/// Both callers wrap the whole dial in one `connection::DIAL_TIMEOUT` (15 s).
/// With no per-address limit, a first address that drops SYN packets used up
/// all 15 s and the next address was never tried, so a dead node that DNS
/// kept listing first held the socket down for as long as it stayed listed.
/// At 5 s a 15 s dial reaches at least the second address, with time left for
/// TLS and the upgrade. A real connect to Dhan takes well under 1 s. The
/// last (or only) address is not held to it: it runs until the caller's
/// `DIAL_TIMEOUT`, as the library dial did (see `connect_first_reachable`).
// APPROVED: this line IS the named constant the no-hardcoded-Duration rule asks for; the scanner matches the declaration itself. Same shape as `connection::DIAL_TIMEOUT`.
pub const PER_ADDRESS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

// Two addresses must fit inside one dial with room for TLS and the upgrade.
const _: () =
    assert!(2 * PER_ADDRESS_CONNECT_TIMEOUT.as_secs() < super::connection::DIAL_TIMEOUT.as_secs());

/// Tries each address in order and returns the first connection made. Every
/// address with another one still to try gets at most `per_address` to
/// connect; one that times out counts as a failed attempt
/// (`ErrorKind::TimedOut`) and the next one is tried. The LAST (or only)
/// address gets no limit of its own: there is nothing to move on to, so
/// cutting it short would only waste the caller's `DIAL_TIMEOUT`, which
/// still bounds it (B2 review round 5, 2026-11-13: a single-address host
/// whose first SYNs are lost connects on the 7 s retransmit, which a 5 s cap
/// refused). Returns the last error when every address fails, as
/// `TcpStream::connect` does. Generic over the connect step so the address
/// walk is testable without a network.
async fn connect_first_reachable<A, T, F, Fut>(
    addrs: A,
    per_address: Duration,
    mut connect: F,
) -> std::io::Result<T>
where
    A: IntoIterator<Item = SocketAddr>,
    F: FnMut(SocketAddr) -> Fut,
    Fut: Future<Output = std::io::Result<T>>,
{
    let mut last_err: Option<std::io::Error> = None;
    let mut addrs = addrs.into_iter().peekable();
    while let Some(addr) = addrs.next() {
        let attempt = if addrs.peek().is_some() {
            match tokio::time::timeout(per_address, connect(addr)).await {
                Ok(result) => result,
                Err(_elapsed) => Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the TCP connect to one resolved address did not finish in its time limit",
                )),
            }
        } else {
            connect(addr).await
        };
        match attempt {
            Ok(stream) => return Ok(stream),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "the host name resolved to no address",
        )
    }))
}

/// Opens one TCP connection to `addr` with `SO_KEEPALIVE` set, and
/// `TCP_NODELAY` when `disable_nagle` is true, both set before the connect.
async fn connect_one_with_keepalive(
    addr: SocketAddr,
    disable_nagle: bool,
) -> std::io::Result<TcpStream> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.set_keepalive(true)?;
    if disable_nagle {
        socket.set_nodelay(true)?;
    }
    socket.connect(addr).await
}

/// Opens a TCP connection to `host:port` with `SO_KEEPALIVE` set, and
/// `TCP_NODELAY` when `disable_nagle` is true, both set before the connect.
///
/// Tries every resolved address in order, each but the last for at most
/// [`PER_ADDRESS_CONNECT_TIMEOUT`], and returns the last error when all fail.
/// The callers still wrap the whole dial (TCP, TLS, upgrade) in one
/// `DIAL_TIMEOUT`.
pub async fn tcp_connect_with_keepalive(
    host: &str,
    port: u16,
    disable_nagle: bool,
) -> std::io::Result<TcpStream> {
    let addrs = tokio::net::lookup_host((host, port)).await?;
    connect_first_reachable(addrs, PER_ADDRESS_CONNECT_TIMEOUT, |addr| {
        connect_one_with_keepalive(addr, disable_nagle)
    })
    .await
}

/// Dials a WebSocket the way `connect_async_tls_with_config` does, except that
/// the TCP socket has kernel keepalive on (see the module header).
///
/// Same arguments in the same order: `config` is the WebSocket config,
/// `disable_nagle` sets `TCP_NODELAY`, `connector` is the TLS connector.
pub async fn dial_ws_tls_with_keepalive(
    request: Request,
    config: Option<WebSocketConfig>,
    disable_nagle: bool,
    connector: Option<Connector>,
) -> Result<(DialedWs, Response), WsError> {
    let port = port_of(&request)?;
    let stream = {
        let host = host_of(&request)?;
        tcp_connect_with_keepalive(host, port, disable_nagle)
            .await
            .map_err(WsError::Io)?
    };
    tokio_tungstenite::client_async_tls_with_config(request, stream, config, connector).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    /// Reads `SO_KEEPALIVE` and `TCP_NODELAY` back from a connected stream.
    fn options_of(stream: TcpStream) -> (bool, bool) {
        let std_stream = stream.into_std().expect("into_std");
        let socket = TcpSocket::from_std_stream(std_stream);
        (
            socket.keepalive().expect("keepalive"),
            socket.nodelay().expect("nodelay"),
        )
    }

    #[tokio::test]
    async fn test_tcp_connect_with_keepalive_sets_so_keepalive() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let accept = tokio::spawn(async move { listener.accept().await });
        let stream = tcp_connect_with_keepalive("127.0.0.1", port, true)
            .await
            .expect("connect");
        let (keepalive, nodelay) = options_of(stream);
        assert!(
            keepalive,
            "SO_KEEPALIVE must be on for every dialled socket"
        );
        assert!(nodelay, "TCP_NODELAY must be on when disable_nagle is true");
        drop(accept.await);
    }

    #[tokio::test]
    async fn test_tcp_connect_with_keepalive_leaves_nagle_when_not_asked() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let accept = tokio::spawn(async move { listener.accept().await });
        let stream = tcp_connect_with_keepalive("127.0.0.1", port, false)
            .await
            .expect("connect");
        let (keepalive, nodelay) = options_of(stream);
        assert!(keepalive);
        assert!(!nodelay);
        drop(accept.await);
    }

    #[test]
    fn test_plain_tokio_connect_does_not_set_keepalive() {
        // The bug this module fixes: the library's dial (a plain
        // `TcpStream::connect`) leaves SO_KEEPALIVE off. If this ever starts
        // failing, the platform changed its default and the helper is still
        // correct, only redundant.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            let accept = tokio::spawn(async move { listener.accept().await });
            let stream = TcpStream::connect(addr).await.expect("connect");
            let (keepalive, _) = options_of(stream);
            assert!(!keepalive);
            drop(accept.await);
        });
    }

    #[tokio::test]
    async fn test_tcp_connect_with_keepalive_refused_port_is_an_error() {
        // Bind then drop, so the port is very likely closed.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        assert!(
            tcp_connect_with_keepalive("127.0.0.1", port, true)
                .await
                .is_err()
        );
    }

    /// B2 review (2026-11-13): a first address whose connect never finishes
    /// (a node that drops SYN packets) must not use up the whole dial. The
    /// walk gives it [`PER_ADDRESS_CONNECT_TIMEOUT`] and moves on, so the
    /// second address is reached inside one `DIAL_TIMEOUT`. Paused time:
    /// the timers advance without a real wait.
    #[tokio::test(start_paused = true)]
    async fn test_a_silent_first_address_does_not_use_up_the_dial() {
        let dead: SocketAddr = "192.0.2.1:443".parse().expect("addr");
        let live: SocketAddr = "192.0.2.2:443".parse().expect("addr");
        let started = tokio::time::Instant::now();
        let walk = connect_first_reachable(
            [dead, live],
            PER_ADDRESS_CONNECT_TIMEOUT,
            |addr| async move {
                if addr == dead {
                    std::future::pending::<std::io::Result<SocketAddr>>().await
                } else {
                    Ok(addr)
                }
            },
        );
        let reached = tokio::time::timeout(super::super::connection::DIAL_TIMEOUT, walk)
            .await
            .expect("the second address is reached inside one dial")
            .expect("connect");
        assert_eq!(reached, live);
        assert_eq!(started.elapsed(), PER_ADDRESS_CONNECT_TIMEOUT);
    }

    /// A refused first address moves on at once; a silent first address
    /// moves on after its limit and the last address's own error is
    /// reported; every address silent is bounded only by the caller's
    /// `DIAL_TIMEOUT`, which still fires.
    #[tokio::test(start_paused = true)]
    async fn test_the_address_walk_reports_timeouts_and_skips_refusals() {
        let a: SocketAddr = "192.0.2.1:443".parse().expect("addr");
        let b: SocketAddr = "192.0.2.2:443".parse().expect("addr");

        let started = tokio::time::Instant::now();
        let err = connect_first_reachable([a, b], PER_ADDRESS_CONNECT_TIMEOUT, |addr| async move {
            if addr == a {
                std::future::pending::<std::io::Result<()>>().await
            } else {
                Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
            }
        })
        .await
        .expect_err("silent then refused");
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
        assert_eq!(started.elapsed(), PER_ADDRESS_CONNECT_TIMEOUT);

        let started = tokio::time::Instant::now();
        let all_silent = tokio::time::timeout(
            super::super::connection::DIAL_TIMEOUT,
            connect_first_reachable([a, b], PER_ADDRESS_CONNECT_TIMEOUT, |_| {
                std::future::pending::<std::io::Result<()>>()
            }),
        )
        .await;
        assert!(all_silent.is_err(), "the caller's dial bound still fires");
        assert_eq!(started.elapsed(), super::super::connection::DIAL_TIMEOUT);

        let started = tokio::time::Instant::now();
        let reached =
            connect_first_reachable([a, b], PER_ADDRESS_CONNECT_TIMEOUT, |addr| async move {
                if addr == a {
                    Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
                } else {
                    Ok(addr)
                }
            })
            .await
            .expect("second");
        assert_eq!(reached, b);
        assert_eq!(started.elapsed(), Duration::ZERO, "a refusal costs no wait");

        let none = connect_first_reachable(
            std::iter::empty::<SocketAddr>(),
            PER_ADDRESS_CONNECT_TIMEOUT,
            |_| async { Ok(()) },
        )
        .await
        .expect_err("no address");
        assert_eq!(none.kind(), std::io::ErrorKind::NotFound);
    }

    /// B2 review round 5 (2026-11-13): the last or only address is not cut
    /// at the per-address limit. A single-address host whose first SYNs are
    /// lost connects on the kernel's 7 s retransmit; that must succeed
    /// inside one `DIAL_TIMEOUT`, as the library dial did. With the 5 s cap
    /// on every address it failed with `TimedOut`.
    #[tokio::test(start_paused = true)]
    async fn test_the_only_address_may_use_the_whole_dial() {
        let only: SocketAddr = "192.0.2.1:443".parse().expect("addr");
        let slow = Duration::from_secs(7);
        assert!(slow > PER_ADDRESS_CONNECT_TIMEOUT);
        assert!(slow < super::super::connection::DIAL_TIMEOUT);
        let started = tokio::time::Instant::now();
        let reached = tokio::time::timeout(
            super::super::connection::DIAL_TIMEOUT,
            connect_first_reachable([only], PER_ADDRESS_CONNECT_TIMEOUT, |addr| async move {
                tokio::time::sleep(slow).await;
                Ok::<_, std::io::Error>(addr)
            }),
        )
        .await
        .expect("inside one dial")
        .expect("the slow single address connects");
        assert_eq!(reached, only);
        assert_eq!(started.elapsed(), slow);
    }

    /// The same for the last of several: after a silent first address used
    /// its 5 s, a slow second address still has the rest of the dial.
    #[tokio::test(start_paused = true)]
    async fn test_the_last_address_after_a_silent_one_is_not_cut_short() {
        let dead: SocketAddr = "192.0.2.1:443".parse().expect("addr");
        let slow_last: SocketAddr = "192.0.2.2:443".parse().expect("addr");
        let started = tokio::time::Instant::now();
        let reached = tokio::time::timeout(
            super::super::connection::DIAL_TIMEOUT,
            connect_first_reachable(
                [dead, slow_last],
                PER_ADDRESS_CONNECT_TIMEOUT,
                |addr| async move {
                    if addr == dead {
                        std::future::pending::<std::io::Result<SocketAddr>>().await
                    } else {
                        tokio::time::sleep(Duration::from_secs(7)).await;
                        Ok(addr)
                    }
                },
            ),
        )
        .await
        .expect("inside one dial")
        .expect("the slow last address connects");
        assert_eq!(reached, slow_last);
        assert_eq!(
            started.elapsed(),
            PER_ADDRESS_CONNECT_TIMEOUT + Duration::from_secs(7)
        );
    }

    #[test]
    fn test_port_and_host_of_follow_the_library_rule() {
        let wss = "wss://api-feed.dhan.co/path"
            .into_client_request()
            .expect("req");
        assert_eq!(port_of(&wss).expect("port"), 443);
        assert_eq!(host_of(&wss).expect("host"), "api-feed.dhan.co");
        let ws = "ws://127.0.0.1:9001".into_client_request().expect("req");
        assert_eq!(port_of(&ws).expect("port"), 9001);
        let v6 = "ws://[::1]:9001".into_client_request().expect("req");
        assert_eq!(host_of(&v6).expect("host"), "::1");
        let plain = "ws://example.com".into_client_request().expect("req");
        assert_eq!(port_of(&plain).expect("port"), 80);
    }

    #[tokio::test]
    async fn test_dial_ws_tls_with_keepalive_upgrades_a_plain_socket() {
        // End to end over a local plain WebSocket server: the dial reaches the
        // upgrade, and the socket under it has SO_KEEPALIVE on.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            tokio_tungstenite::accept_async(tcp).await.map(|_| ())
        });
        let request = format!("ws://127.0.0.1:{port}/")
            .into_client_request()
            .expect("req");
        let (ws, _resp) = dial_ws_tls_with_keepalive(
            request,
            Some(WebSocketConfig::default()),
            true,
            Some(Connector::Plain),
        )
        .await
        .expect("dial");
        let tcp = match ws.into_inner() {
            MaybeTlsStream::Plain(tcp) => tcp,
            _ => panic!("expected a plain stream"),
        };
        let (keepalive, nodelay) = options_of(tcp);
        assert!(keepalive);
        assert!(nodelay);
        server.await.expect("join").expect("server handshake");
    }
}
